//! The Codex provider: `codex app-server`, driven through the shared transport.
//!
//! This is the layer between [`crate::provider::Provider`] and the wire. It owns
//! three things: where the CLI is, whether it is usable, and how one session's
//! requests are shaped.
//!
//! Health comes from a one-shot command. Catalog discovery uses the supported
//! `model/list` app-server endpoint without opening a conversation. Discovery
//! failures retain the last usable catalog, so a missing connection does not
//! empty the user's model picker.

use std::time::Duration;

use fx_core::services::runtime::{BinaryHealth, BinarySpec, RuntimeDiscovery, RuntimeLaunch};
use serde_json::{Value, json};

use crate::codex::session::{
    fallback_models, list_threads_params, parse_model_list, threads_from_result,
};
use crate::codex::transport::{Launch, Transport};
use crate::provider::{
    Model, Provider, ProviderError, ProviderHealth, ThreadIdentity, ThreadSummary,
};

/// How long a one-shot command may take.
///
/// Generous enough for a cold model list on a busy machine, short enough that a
/// wedged CLI does not leave the window showing nothing.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Most threads requested per page.
pub const THREAD_PAGE_SIZE: u64 = 50;

/// Most pages fetched for one listing.
///
/// A cap, so a directory with thousands of threads cannot spin the UI forever.
pub const MAX_THREAD_PAGES: usize = 20;

/// What FlowX calls itself, as the server sees it.
pub const CLIENT_NAME: &str = "FlowX";

/// A Codex install and a live session factory.
pub struct CodexProvider {
    discovery: RuntimeDiscovery,
    spec: BinarySpec,
    health: BinaryHealth,
    models: Vec<Model>,
    client_version: String,
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProvider")
            .field("spec", &self.spec.id)
            .field("health", &self.health)
            .field("models", &self.models.len())
            .finish()
    }
}

impl CodexProvider {
    /// A provider using the default `codex` spec.
    pub fn new(discovery: RuntimeDiscovery) -> Self {
        Self::with_spec(discovery, fx_core::services::runtime::codex_spec())
    }

    /// A provider for a specific spec, which is how tests point it at a fixture.
    pub fn with_spec(discovery: RuntimeDiscovery, spec: BinarySpec) -> Self {
        Self {
            discovery,
            spec,
            // The catalogue is never empty: a CLI that cannot start still has to
            // offer something, or the model picker is empty with no explanation.
            health: BinaryHealth::Checking,
            models: fallback_models(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn spec(&self) -> &BinarySpec {
        &self.spec
    }

    pub fn binary_health(&self) -> &BinaryHealth {
        &self.health
    }

    /// Re-check the install and the model catalogue.
    ///
    /// Safe to call at any time. A failure is recorded rather than raised: the
    /// caller wants to show a disabled control with a reason, not to handle an
    /// error, and `models` keeps the last good list either way.
    pub async fn refresh(&mut self) {
        self.health = self.discovery.health(&self.spec).await;
        self.models = self.fetch_models().await;
    }

    /// Discover the account's picker-visible catalog through the supported RPC.
    /// The whole operation has one deadline, including handshake and pagination.
    pub async fn discover_models(&self) -> Result<Vec<Model>, ProviderError> {
        tokio::time::timeout(PROBE_TIMEOUT, async {
            let connection = MetadataConnection::open(self).await?;
            let result = list_models_with(&connection.transport).await;
            connection.close().await;
            result
        })
        .await
        .map_err(|_| ProviderError::Unavailable("Codex model discovery timed out.".into()))?
    }

    async fn fetch_models(&mut self) -> Vec<Model> {
        self.discover_models()
            .await
            .unwrap_or_else(|_| self.models.clone())
    }

    /// The version string, for the settings row.
    pub async fn version(&self) -> Option<String> {
        let result = self
            .discovery
            .run(&self.spec, &self.spec.version_args, None, PROBE_TIMEOUT)
            .await
            .ok()?;
        result.first_line().map(str::to_string)
    }

    /// Start a session, or say why one cannot be started.
    pub async fn start_session(&self) -> Result<Transport, ProviderError> {
        let launch = self.launch().await?;
        Transport::start(launch, CLIENT_NAME, &self.client_version)
            .await
            .map_err(map_transport_error)
    }

    /// The resolved launch, for a caller that wants to start the process itself.
    pub async fn launch(&self) -> Result<Launch, ProviderError> {
        let runtime = self
            .discovery
            .launch(&self.spec)
            .await
            .map_err(map_discovery_error)?;
        Ok(launch_from_runtime(&runtime, &self.spec))
    }

    /// The launch specification, before resolution.
    pub fn launch_spec(&self) -> &BinarySpec {
        &self.spec
    }
}

/// A short-lived connection for one metadata request.
///
/// Nobody reads a metadata connection's notifications, and the queue they land
/// in is bounded: once more than it holds arrive ahead of the reply, the
/// reader stops, the reply is never read, and the request waits out its
/// timeout. So the queue is drained for the life of the connection, server
/// requests are refused rather than left blocking the server, and the process
/// is closed afterwards instead of lingering until it is dropped.
struct MetadataConnection {
    transport: std::sync::Arc<Transport>,
    drain: tokio::task::JoinHandle<()>,
}

impl MetadataConnection {
    async fn open(provider: &CodexProvider) -> Result<Self, ProviderError> {
        let mut transport = provider.start_session().await?;
        let mut events = transport.take_events();
        let transport = std::sync::Arc::new(transport);
        let weak = std::sync::Arc::downgrade(&transport);
        let drain = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let crate::codex::transport::ServerEvent::Request { id, .. } = event
                    && let Some(transport) = weak.upgrade()
                {
                    let _ = transport.refuse(
                        id,
                        -32601,
                        "A metadata request does not run tools or answer prompts.",
                    );
                }
            }
        });
        Ok(Self { transport, drain })
    }

    async fn close(self) {
        self.transport.close().await;
        self.drain.abort();
    }
}

/// Read each catalog page, bounded both by the caller deadline and page count.
/// This never opens a conversation or sends a user prompt.
pub async fn list_models_with(transport: &Transport) -> Result<Vec<Model>, ProviderError> {
    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..20 {
        let page = transport
            .request(
                "model/list",
                Some(json!({
                    "limit": 100, "includeHidden": false, "cursor": cursor,
                })),
            )
            .await
            .map_err(map_transport_error)?;
        if !page.get("data").is_some_and(Value::is_array) {
            return Err(ProviderError::Protocol(
                "Codex returned an invalid model catalog.".into(),
            ));
        }
        for model in parse_model_list(&page, &fallback_models()) {
            if !models.iter().any(|known: &Model| known.id == model.id) {
                models.push(model);
            }
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let Some(next) = cursor.as_ref() else {
            return if models.is_empty() {
                Err(ProviderError::Unavailable(
                    "Codex did not list any available models.".into(),
                ))
            } else {
                Ok(models)
            };
        };
        if !seen.insert(next.clone()) {
            return Err(ProviderError::Protocol(
                "Codex repeated a model catalog page.".into(),
            ));
        }
    }
    Err(ProviderError::Protocol(
        "Codex model discovery exceeded its page limit.".into(),
    ))
}

/// A resolved runtime, shaped for the transport.
///
/// The `app-server` subcommand is appended here rather than in the spec so the
/// spec stays a description of the *binary* and this stays a description of the
/// *protocol session* it hosts.
fn launch_from_runtime(runtime: &RuntimeLaunch, spec: &BinarySpec) -> Launch {
    let mut args = spec.arguments.clone();
    args.push("app-server".to_string());
    Launch {
        executable: runtime.executable.to_string_lossy().into_owned(),
        args,
        working_directory: None,
        environment: runtime
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    }
}

fn map_discovery_error(error: fx_core::services::runtime::DiscoveryError) -> ProviderError {
    use fx_core::services::runtime::DiscoveryError;
    match error {
        DiscoveryError::NotFound { .. } => ProviderError::Unavailable(error.to_string()),
        DiscoveryError::Quarantined { .. } => ProviderError::Unavailable(error.to_string()),
        DiscoveryError::LaunchFailed { .. } => ProviderError::Unavailable(error.to_string()),
    }
}

fn map_transport_error(error: crate::codex::transport::TransportError) -> ProviderError {
    use crate::codex::transport::TransportError;
    match error {
        // Cancelled is not a failure here; this is a start-up path.
        TransportError::Rejected { message, .. } => ProviderError::Protocol(message),
        TransportError::Spawn(message) | TransportError::Write(message) => {
            ProviderError::Unavailable(message)
        }
        TransportError::Exited { stderr } => ProviderError::Unavailable(stderr),
        TransportError::TimedOut { method } => {
            ProviderError::Unavailable(format!("Codex did not answer `{method}` in time."))
        }
        other => ProviderError::Unavailable(other.to_string()),
    }
}

#[async_trait::async_trait]
impl Provider for CodexProvider {
    fn id(&self) -> &str {
        "codex"
    }

    fn health(&self) -> ProviderHealth {
        match &self.health {
            BinaryHealth::Available { path, version } => ProviderHealth::Available {
                path: path.clone(),
                version: version.clone().unwrap_or_default(),
            },
            BinaryHealth::Quarantined { path } => {
                ProviderHealth::Quarantined { path: path.clone() }
            }
            BinaryHealth::NotFound => ProviderHealth::NotFound,
            BinaryHealth::Checking => ProviderHealth::Unknown,
        }
    }

    fn models(&self) -> Vec<Model> {
        self.models.clone()
    }

    async fn list_threads(
        &self,
        working_directory: &str,
    ) -> Result<Vec<ThreadSummary>, ProviderError> {
        let connection = MetadataConnection::open(self).await?;
        let result = list_threads_with(&connection.transport, working_directory).await;
        connection.close().await;
        result
    }

    async fn read_thread(&self, identity: &ThreadIdentity) -> Result<String, ProviderError> {
        let connection = MetadataConnection::open(self).await?;
        let result = connection
            .transport
            .request(
                "thread/read",
                Some(json!({ "threadId": identity.session_id })),
            )
            .await
            .map_err(map_transport_error);
        connection.close().await;
        Ok(thread_transcript(&result?))
    }
}

/// Read the thread list, following pagination.
///
/// A CLI old enough not to have `thread/list` answers "method not found"; that
/// is an empty list, not an error, because a missing feature is not a failure.
pub async fn list_threads_with(
    transport: &Transport,
    working_directory: &str,
) -> Result<Vec<ThreadSummary>, ProviderError> {
    let mut threads = Vec::new();
    let mut cursor: Option<String> = None;

    for _ in 0..MAX_THREAD_PAGES {
        let params = list_threads_params(
            &[working_directory.to_string()],
            THREAD_PAGE_SIZE,
            false,
            false,
            cursor.as_deref(),
        );
        let result = match transport.request("thread/list", Some(params)).await {
            Ok(result) => result,
            Err(crate::codex::transport::TransportError::Rejected { code, message, .. })
                if code == crate::codex::protocol::METHOD_NOT_FOUND
                    || is_unavailable_text(&message) =>
            {
                return Ok(threads);
            }
            Err(error) => return Err(map_transport_error(error)),
        };

        for raw in threads_from_result(&result) {
            if let Some(summary) = thread_summary(&raw) {
                threads.push(summary);
            }
        }

        match crate::codex::session::next_cursor(&result) {
            // A cursor that repeats would page forever.
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }

    Ok(threads)
}

/// Whether an error message means "this build is too old".
///
/// The code is the reliable signal; the text is a fallback for a server that
/// reports an unusual code with the usual wording.
fn is_unavailable_text(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("method not found")
        || message.contains("unsupported method")
        || message.contains("unknown method")
        || message.contains("not implemented")
}

/// One listed thread.
pub fn thread_summary(raw: &Value) -> Option<ThreadSummary> {
    let session_id = raw
        .get("id")
        .or_else(|| raw.get("threadId"))
        .and_then(Value::as_str)?
        .to_string();
    if session_id.is_empty() {
        return None;
    }
    Some(ThreadSummary {
        identity: ThreadIdentity {
            provider_id: "codex".to_string(),
            session_id,
        },
        // Codex calls it `name` in the live payload and `title` in some older
        // responses, so both are read rather than guessing one.
        title: raw
            .get("name")
            .or_else(|| raw.get("title"))
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_default(),
        working_directory: raw
            .get("cwd")
            .or_else(|| raw.get("workingDirectory"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: thread_timestamp(raw.get("updatedAt").and_then(Value::as_i64).unwrap_or(0)),
        is_archived: raw
            .get("archived")
            .or_else(|| raw.get("isArchived"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// A thread timestamp.
///
/// Codex sends *seconds* in `updatedAt`, not milliseconds — confirmed against
/// `codex app-server` 0.155.1, where a same-day thread read back as
/// `1791486055`. Reading that as milliseconds put the thread in the year 58656.
///
/// Both are accepted because a build that switches units would otherwise show
/// every thread as absurdly old or new, and the threshold below is far enough
/// apart that no plausible value is ambiguous.
fn thread_timestamp(value: i64) -> fx_core::Timestamp {
    const SECONDS_CEILING: i64 = 100_000_000_000; // year 5138, in seconds
    // `unsigned_abs`, not `abs`: `i64::MIN.abs()` overflows, and the value
    // arrives straight from a server payload.
    if value.unsigned_abs() < SECONDS_CEILING.unsigned_abs() {
        chrono::DateTime::from_timestamp(value, 0).unwrap_or_default()
    } else {
        chrono::DateTime::from_timestamp_millis(value).unwrap_or_default()
    }
}

/// The readable transcript from a `thread/read` result.
///
/// A thread is a list of turns, each with items. Only the text is kept: the
/// full item stream is what the live parser is for, and reading a saved thread
/// wants prose.
pub fn thread_transcript(result: &Value) -> String {
    let turns = result
        .get("thread")
        .and_then(|thread| thread.get("turns"))
        .and_then(Value::as_array)
        .or_else(|| result.get("turns").and_then(Value::as_array));

    let Some(turns) = turns else {
        return result
            .get("transcript")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    };

    let mut out = String::new();
    for turn in turns {
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(text) = item_text(item) else {
                continue;
            };
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
    }
    out
}

fn item_text(item: &Value) -> Option<String> {
    let kind = item.get("type").and_then(Value::as_str)?;
    let text = match kind {
        "agentMessage" | "assistantMessage" | "message" => item
            .get("text")
            .or_else(|| item.get("content"))
            .and_then(Value::as_str)?,
        "userMessage" => item.get("text").and_then(Value::as_str)?,
        // A tool call is not prose, but its command is what the user wants to
        // see when reading back a thread.
        "commandExecution" => {
            let command = item.get("command").and_then(Value::as_str)?;
            return Some(format!("`{command}`"));
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::fake_server::FakeServer;
    use crate::codex::transport::Transport;
    use fx_core::services::runtime::BinarySpec;

    #[tokio::test]
    async fn model_discovery_pages_account_choices_without_starting_a_thread() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = Transport::start(server.launch(), "FlowX", "test")
            .await
            .unwrap();
        let models = list_models_with(&transport).await.unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["model-first", "model-second"]
        );
        assert!(models[0].is_default);
        assert_eq!(models[0].supported_efforts, ["high"]);
        assert!(!models[1].supports_vision);
        transport.close().await;
    }

    #[test]
    fn a_provider_starts_with_a_usable_model_list() {
        // The picker must never open empty, even before anything is discovered.
        let provider = CodexProvider::new(RuntimeDiscovery::default());
        assert!(!provider.models().is_empty());
        assert_eq!(provider.id(), "codex");
    }

    #[test]
    fn an_unlaunchable_provider_reports_unavailable() {
        let mut provider =
            CodexProvider::new(RuntimeDiscovery::default().without_extra_directories());
        provider.health = BinaryHealth::NotFound;
        assert!(matches!(provider.health(), ProviderHealth::NotFound));
        assert!(!provider.health().is_usable());
    }

    #[test]
    fn a_quarantined_install_is_reported_distinctly_from_a_missing_one() {
        // Different instruction for the user: approve it, or install it.
        let mut provider = CodexProvider::new(RuntimeDiscovery::default());
        provider.health = BinaryHealth::Quarantined {
            path: "/tmp/codex".into(),
        };
        assert!(matches!(
            provider.health(),
            ProviderHealth::Quarantined { .. }
        ));
        assert_eq!(provider.health().path(), Some("/tmp/codex"));
    }

    #[test]
    fn a_launch_asks_for_the_app_server_subcommand() {
        // The spec describes the binary; the subcommand belongs to the protocol
        // session it hosts.
        let spec = BinarySpec::new("codex", "Codex", vec!["/usr/local/bin/codex".into()]);
        let launch = launch_from_runtime(
            &RuntimeLaunch {
                executable: "/usr/local/bin/codex".into(),
                environment: Default::default(),
            },
            &spec,
        );
        assert_eq!(launch.executable, "/usr/local/bin/codex");
        assert_eq!(launch.args, vec!["app-server".to_string()]);
    }

    #[tokio::test]
    async fn a_session_starts_through_the_provider() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        // A spec names a path, not a command, so the interpreter is resolved.
        let python = std::process::Command::new("/usr/bin/env")
            .arg("which")
            .arg("python3")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .expect("python3 is on PATH");
        let script = server.script_path().to_string_lossy().into_owned();
        let spec = BinarySpec::new("codex", "Codex", vec![python]).with_arguments(vec![script]);
        let provider = CodexProvider::with_spec(
            RuntimeDiscovery::default().without_extra_directories(),
            spec,
        );
        let transport = provider
            .start_session()
            .await
            .expect("a session over the fake server");
        transport.close().await;
        drop(provider);
    }

    #[tokio::test]
    async fn metadata_requests_are_answered_through_a_notification_flood() {
        // More notifications ahead of the reply than the transport queues.
        // Nothing drained them, so the reader stopped with the reply unread and
        // every metadata request waited out its timeout.
        let Some(server) = FakeServer::new() else {
            return;
        };
        std::fs::write(server.file("flood"), "").unwrap();
        let python = std::process::Command::new("/usr/bin/env")
            .arg("which")
            .arg("python3")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .expect("python3 is on PATH");
        let script = server.script_path().to_string_lossy().into_owned();
        let provider = CodexProvider::with_spec(
            RuntimeDiscovery::default().without_extra_directories(),
            BinarySpec::new("codex", "Codex", vec![python]).with_arguments(vec![script]),
        );
        let budget = std::time::Duration::from_secs(10);

        let models = tokio::time::timeout(budget, provider.discover_models())
            .await
            .expect("model discovery was not stalled")
            .expect("models");
        assert_eq!(models.len(), 2, "{models:?}");
        let threads = tokio::time::timeout(budget, provider.list_threads("/tmp/project"))
            .await
            .expect("the listing was not stalled")
            .expect("threads");
        assert_eq!(threads.len(), 1, "{threads:?}");
        let identity = ThreadIdentity {
            provider_id: "codex".into(),
            session_id: "thread-a".into(),
        };
        let transcript = tokio::time::timeout(budget, provider.read_thread(&identity))
            .await
            .expect("the read was not stalled")
            .expect("a transcript");
        assert_eq!(transcript, "hello\n\nhi");
    }

    #[tokio::test]
    async fn an_absent_cli_cannot_start_a_session() {
        let spec = BinarySpec::new("codex", "Codex", vec!["/nonexistent/codex".into()]);
        let provider = CodexProvider::with_spec(
            RuntimeDiscovery::default().without_extra_directories(),
            spec,
        );
        assert!(matches!(
            provider.start_session().await,
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_live_server_lists_its_threads() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_fake(&server).await;
        let threads = list_threads_with(&transport, "/tmp/project")
            .await
            .expect("the list succeeded");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].identity.provider_id, "codex");
        assert_eq!(threads[0].title, "A thread");
        assert_eq!(threads[0].working_directory, "/tmp/project");
        assert!(!threads[0].is_archived);
        transport.close().await;
    }

    #[tokio::test]
    async fn a_server_without_thread_list_returns_an_empty_list() {
        // An older CLI answering "method not found" is a missing feature, not a
        // failure.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_with(&server, &[("FAKE_NO_LIST", "1")]).await;
        let threads = list_threads_with(&transport, "/tmp/project")
            .await
            .expect("an empty list, not an error");
        assert!(threads.is_empty(), "{threads:?}");
        transport.close().await;
    }

    #[tokio::test]
    async fn a_server_that_dies_mid_listing_reports_the_exit() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_fake(&server).await;
        transport
            .notify(crate::codex::protocol::Outgoing::Notification {
                method: "die".into(),
                params: None,
            })
            .expect("written");
        let error = list_threads_with(&transport, "/tmp/project")
            .await
            .expect_err("the server had gone");
        assert!(
            matches!(error, ProviderError::Unavailable(_)),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn a_paging_server_is_followed_to_the_end() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_with(&server, &[("FAKE_PAGES", "page-1")]).await;
        let threads = list_threads_with(&transport, "/tmp/project")
            .await
            .expect("every page");
        let ids: Vec<&str> = threads
            .iter()
            .map(|thread| thread.identity.session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["thread-a", "thread-c", "thread-b"],
            "the pages were not followed in order"
        );
        transport.close().await;
    }

    #[tokio::test]
    async fn a_server_that_repeats_its_cursor_is_not_paged_forever() {
        // The failure this guards: a cursor that never advances spins the UI.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_with(&server, &[("FAKE_PAGES", "page-repeats")]).await;
        let threads = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            list_threads_with(&transport, "/tmp/project"),
        )
        .await
        .expect("paging stopped rather than spinning")
        .expect("listed");
        assert!(!threads.is_empty());
        transport.close().await;
    }

    #[tokio::test]
    async fn a_session_hands_the_runtime_its_own_path() {
        // The provider cannot be launched from Finder's PATH, so the transport
        // has to hand the child one that includes the discovered directory.
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_with(&server, &[("FLOWX_PROBE", "1")]).await;
        let seen = transport
            .request("env", None)
            .await
            .expect("the server answered");
        assert!(
            seen["PATH"].as_str().is_some_and(|path| !path.is_empty()),
            "the child was launched with no PATH: {seen}"
        );
        transport.close().await;
    }

    #[tokio::test]
    async fn a_thread_transcript_is_read_over_the_wire() {
        let Some(server) = FakeServer::new() else {
            return;
        };
        let transport = start_fake(&server).await;
        let result = transport
            .request("thread/read", Some(json!({ "threadId": "thread-a" })))
            .await
            .expect("the read succeeded");
        assert_eq!(thread_transcript(&result), "hello\n\nhi");
        transport.close().await;
    }

    #[test]
    fn a_transcript_reads_its_text_and_its_commands() {
        let transcript = thread_transcript(&json!({
            "thread": { "turns": [
                { "items": [
                    { "type": "userMessage", "text": "do the thing" },
                    { "type": "commandExecution", "command": "cargo test" },
                    { "type": "reasoning", "text": "thinking" }
                ]},
                { "items": [
                    { "type": "agentMessage", "text": "done" }
                ]}
            ]}
        }));
        assert_eq!(transcript, "do the thing\n\n`cargo test`\n\ndone");
    }

    #[test]
    fn a_transcript_with_no_turns_is_empty_rather_than_absent() {
        assert_eq!(thread_transcript(&json!({})), "");
        assert_eq!(thread_transcript(&json!({"turns": []})), "");
    }

    #[test]
    fn an_empty_item_text_is_skipped() {
        let transcript = thread_transcript(&json!({
            "turns": [{ "items": [{ "type": "agentMessage", "text": "   " }] }]
        }));
        assert_eq!(transcript, "");
    }

    #[test]
    fn a_listed_thread_without_an_id_is_skipped() {
        assert!(thread_summary(&json!({ "title": "no id" })).is_none());
        assert!(thread_summary(&json!({ "id": "" })).is_none());
    }

    #[test]
    fn a_live_thread_shape_is_read_correctly() {
        // Captured verbatim from `codex app-server` 0.155.1. The invented shape
        // the fixture used had `title` and millisecond `updatedAt`, both of which
        // are wrong against the real server.
        let raw = json!({
            "agentNickname": null,
            "cliVersion": "0.162.0-alpha.2",
            "cwd": "/Users/someone/code",
            "ephemeral": false,
            "historyMode": "paginated",
            "id": "01a11ccc-c4c4-73a2-b543-c897dc9ef01e",
            "model": "gpt-6-astra",
            "name": "Find Micauto August investment",
            "originator": "Codex Desktop",
            "path": "/Users/someone/.codex/sessions/rollout.jsonl",
            "recencyAt": 1791485339,
            "sessionId": "01a11ccc-c4c4-73a2-b543-c897dc9ef01e",
            "status": { "type": "notLoaded" },
            "turns": [],
            "updatedAt": 1791486055
        });
        let summary = thread_summary(&raw).expect("the real shape parses");
        assert_eq!(
            summary.title, "Find Micauto August investment",
            "the live field is `name`, not `title`"
        );
        assert_eq!(summary.working_directory, "/Users/someone/code");
        assert_eq!(
            summary.identity.session_id,
            "01a11ccc-c4c4-73a2-b543-c897dc9ef01e"
        );
        assert!(!summary.is_archived);

        // Seconds, not milliseconds: this thread is from 2026, not the year 58656.
        let updated = summary.updated_at.timestamp();
        assert!(
            (1_700_000_000..1_900_000_000).contains(&updated),
            "the timestamp was read with the wrong unit: {updated}"
        );
    }

    #[test]
    fn a_millisecond_timestamp_is_still_read() {
        // A build that reports milliseconds must not look like the year 1970.
        let millis = 1_791_486_055_000;
        let seconds = thread_timestamp(1_791_486_055);
        let parsed = thread_timestamp(millis);
        assert_eq!(parsed.timestamp(), seconds.timestamp());
    }

    #[test]
    fn a_thread_without_a_title_is_untitled_rather_than_missing() {
        let summary = thread_summary(&json!({ "id": "t1" })).expect("parsed");
        assert_eq!(summary.title, "");
    }

    #[test]
    fn an_extreme_timestamp_does_not_panic() {
        // `i64::MIN.abs()` overflows, and this value comes from a payload.
        for value in [i64::MIN, i64::MAX, 0, -1, i64::MIN + 1] {
            let _ = thread_timestamp(value);
        }
    }

    #[test]
    fn a_thread_without_a_timestamp_does_not_panic() {
        let summary = thread_summary(&json!({ "id": "t1" })).expect("parsed");
        assert_eq!(summary.updated_at.timestamp_millis(), 0);
    }

    #[test]
    fn an_old_build_is_recognised_by_code_or_by_wording() {
        assert!(is_unavailable_text("Method not found"));
        assert!(is_unavailable_text("unsupported method"));
        assert!(is_unavailable_text("not implemented"));
        assert!(!is_unavailable_text("connection reset"));
    }

    /// Start the fake server through the real transport path.
    async fn start_fake(server: &FakeServer) -> Transport {
        start_with(server, &[]).await
    }

    /// Start the fake server with its behaviour pinned by environment.
    async fn start_with(server: &FakeServer, environment: &[(&str, &str)]) -> Transport {
        Transport::start(server.launch_with_env(environment), CLIENT_NAME, "test")
            .await
            .unwrap_or_else(|error| panic!("the fake server did not start: {error}"))
    }
}
