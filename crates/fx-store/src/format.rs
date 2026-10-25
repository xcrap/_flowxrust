//! The on-disk shape.
//!
//! Deliberately not a mirror of the in-memory types. The persisted form is
//! allowed to be flatter, to carry migration fields, and to validate what it
//! reads — the in-memory model should not have to know any of that.
//!
//! The rule this file follows throughout: **decoding must never fail because of
//! one bad value.** An unknown enum value reads as its default. A project,
//! thread, workspace, or binding that does not decode at all is set aside and
//! hidden, not dropped: it is written back unchanged on the next save, so a
//! file shared with a newer build does not lose what only that build can read.

use std::collections::BTreeMap;

use fx_core::{Agent, AgentAccess, ConversationMessage, Project, Timestamp};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest project file that will be read, in bytes.
pub const MAX_PROJECTS_BYTES: u64 = 16 * 1024 * 1024;
/// Largest conversation file that will be read, in bytes.
pub const MAX_CONVERSATION_BYTES: u64 = 32 * 1024 * 1024;
/// Most dormant provider presentations remembered per project.
pub const MAX_DORMANT_PRESENTATIONS: usize = 32;

/// Choices copied into new chats for one provider. Unset fields leave the
/// provider's own model/reasoning selection and FlowX's access default intact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderDefaults {
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_or_default"
    )]
    pub model_id: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_or_default"
    )]
    pub effort: Option<String>,
    /// A level this build cannot read asks first, as a thread's own does:
    /// unset would fall to the app default, which is the widest.
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "fx_core::models::agent::access_or_inherit"
    )]
    pub agent_access: Option<AgentAccess>,
}

/// One lightweight provider and model used to name every newly created chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoTitlePreferences {
    pub enabled: bool,
    pub provider_id: String,
    pub model_id: String,
}

impl Default for AutoTitlePreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            provider_id: "codex".into(),
            model_id: Self::default_model_id("codex").into(),
        }
    }
}

impl AutoTitlePreferences {
    pub fn default_model_id(provider: &str) -> &'static str {
        match provider {
            "claude" => "haiku",
            _ => "gpt-5.6-luna",
        }
    }
}

impl<'de> Deserialize<'de> for AutoTitlePreferences {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mut result = Self::default();
        if let Some(enabled) = value.get("enabled").and_then(|value| value.as_bool()) {
            result.enabled = enabled;
        }
        let provider_value = value.get("provider_id");
        let provider = provider_value.and_then(|value| value.as_str());
        if let Some(provider @ ("codex" | "claude")) = provider {
            result.provider_id = provider.into();
        }
        // A future/invalid provider must not carry its model into Codex. Older
        // preferences used one model per chat provider; preserve the Codex one
        // as the shared choice when migrating that format.
        if provider_value.is_none() || matches!(provider, Some("codex" | "claude")) {
            let legacy_key = if result.provider_id == "claude" {
                "claude_model_id"
            } else {
                "codex_model_id"
            };
            result.model_id = value
                .get("model_id")
                .and_then(|value| value.as_str())
                .filter(|model| !model.trim().is_empty())
                .or_else(|| {
                    value
                        .get(legacy_key)
                        .and_then(|value| value.as_str())
                        .filter(|model| !model.trim().is_empty())
                })
                .unwrap_or_else(|| Self::default_model_id(&result.provider_id))
                .into();
        }
        Ok(result)
    }
}

fn deserialize_or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

/// List elements that did not decode, kept verbatim for the next save.
///
/// Skipping them would be quieter, and the next save would then delete them:
/// a thread written by a newer build would be gone by the time that build read
/// the file again. They are never shown, only carried, and land at the end of
/// their list when written back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Unreadable(BTreeMap<&'static str, Vec<serde_json::Value>>);

impl Unreadable {
    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }

    /// The elements of one list, by its key in the file.
    pub fn elements(&self, key: &str) -> &[serde_json::Value] {
        self.0.get(key).map_or(&[], Vec::as_slice)
    }

    /// Remove `key`'s list from `object` and decode it one element at a time.
    ///
    /// A value that is not a list is left where it is, for the strict decode to
    /// refuse: there is no element to keep, and guessing would lose it.
    fn take<T: serde::de::DeserializeOwned>(
        &mut self,
        object: &mut serde_json::Value,
        key: &'static str,
    ) -> Vec<T> {
        let Some(map) = object.as_object_mut() else {
            return Vec::new();
        };
        if !map.get(key).is_some_and(serde_json::Value::is_array) {
            return Vec::new();
        }
        let Some(serde_json::Value::Array(elements)) = map.remove(key) else {
            return Vec::new();
        };
        let mut readable = Vec::with_capacity(elements.len());
        for element in elements {
            match T::deserialize(&element) {
                Ok(value) => readable.push(value),
                Err(error) => {
                    tracing::warn!(list = key, %error, "keeping a saved entry this build cannot read");
                    self.0.entry(key).or_default().push(element);
                }
            }
        }
        readable
    }

    /// Append every kept element to its list in `object`.
    fn restore(&self, object: &mut serde_json::Value) {
        let Some(map) = object.as_object_mut() else {
            return;
        };
        for (key, elements) in &self.0 {
            if elements.is_empty() {
                continue;
            }
            let list = map
                .entry(*key)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let serde_json::Value::Array(list) = list {
                list.extend(elements.iter().cloned());
            }
        }
    }

    /// Serialize through the derived impl, then put the kept elements back.
    fn write_back<S: serde::Serializer>(
        &self,
        derived: Result<serde_json::Value, serde_json::Error>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut value = derived.map_err(serde::ser::Error::custom)?;
        self.restore(&mut value);
        value.serialize(serializer)
    }
}

// `Self::deserialize` and `Self::serialize` below are the derived, strict impls
// that `remote = "Self"` generates as inherent functions. Calling the trait
// instead would recurse.

impl<'de> Deserialize<'de> for PersistedApp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let mut unreadable = Unreadable::default();
        let projects = unreadable.take(&mut value, "projects");
        let mut app = Self::deserialize(value).map_err(serde::de::Error::custom)?;
        app.projects = projects;
        app.unreadable = unreadable;
        Ok(app)
    }
}

impl Serialize for PersistedApp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.unreadable.is_empty() {
            return Self::serialize(self, serializer);
        }
        let derived = Self::serialize(self, serde_json::value::Serializer);
        self.unreadable.write_back(derived, serializer)
    }
}

impl<'de> Deserialize<'de> for PersistedProject {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let mut unreadable = Unreadable::default();
        let agents = unreadable.take(&mut value, "agents");
        let workspaces = unreadable.take(&mut value, "workspaces");
        let native_bindings = unreadable.take(&mut value, "native_bindings");
        let mut record = Self::deserialize(value).map_err(serde::de::Error::custom)?;
        record.agents = agents;
        record.workspaces = workspaces;
        record.native_bindings = native_bindings;
        record.unreadable = unreadable;
        Ok(record)
    }
}

impl Serialize for PersistedProject {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.unreadable.is_empty() {
            return Self::serialize(self, serializer);
        }
        let derived = Self::serialize(self, serde_json::value::Serializer);
        self.unreadable.write_back(derived, serializer)
    }
}

/// User preferences use stable strings so storage stays independent of UI types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedPreferences {
    pub appearance: String,
    pub base_tone: String,
    pub accent: String,
    pub text_size: String,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub codex_defaults: ProviderDefaults,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub claude_defaults: ProviderDefaults,
    #[serde(deserialize_with = "deserialize_or_default")]
    pub auto_titles: AutoTitlePreferences,
    /// What a prompt sent while a turn runs does: `"steer"` guides the running
    /// turn, `"queue"` waits for it. Steering by default, as the original.
    pub follow_up_mode: String,
}

impl Default for PersistedPreferences {
    fn default() -> Self {
        Self {
            appearance: "dark".into(),
            base_tone: "zinc".into(),
            accent: "violet".into(),
            text_size: "standard".into(),
            codex_defaults: ProviderDefaults::default(),
            claude_defaults: ProviderDefaults::default(),
            auto_titles: AutoTitlePreferences::default(),
            follow_up_mode: "steer".into(),
        }
    }
}

// ── Projects ───────────────────────────────────────────────────────────────

/// The project and layout snapshot.
///
/// The derives are `remote = "Self"` so the hand-written impls below can wrap
/// them: the `projects` list is decoded one element at a time first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct PersistedApp {
    #[serde(default)]
    pub projects: Vec<PersistedProject>,
    /// Projects this build could not read, written back unchanged.
    #[serde(skip)]
    pub unreadable: Unreadable,
    #[serde(rename = "activeProjectId")]
    pub active_project_id: Option<Uuid>,
    #[serde(rename = "activeAgentId")]
    pub active_agent_id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
    #[serde(
        rename = "sidebarWidth",
        default = "default_sidebar_width",
        deserialize_with = "deserialize_sidebar_width"
    )]
    pub sidebar_width: f32,
    #[serde(default)]
    pub right_panel_visible: bool,
    #[serde(rename = "rightPanelWidth", default = "default_panel_width")]
    pub right_panel_width: f32,
    /// Narrowest and widest the inspector may be dragged.
    #[serde(rename = "minRightPanelWidth", default = "default_min_panel_width")]
    pub min_right_panel_width: f32,
    #[serde(rename = "maxRightPanelWidth", default = "default_max_panel_width")]
    pub max_right_panel_width: f32,
    /// A tab a newer build added reads as the default. Decoded strictly, one
    /// unknown name failed the whole snapshot, and every project vanished.
    #[serde(
        rename = "rightPanelTab",
        default,
        deserialize_with = "deserialize_or_default"
    )]
    pub right_panel_tab: RightPanelTab,
    /// Where the main window was. Absent until the window first moves.
    #[serde(
        rename = "windowFrame",
        default,
        deserialize_with = "deserialize_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub window_frame: Option<crate::window::PersistedWindowFrame>,
}

/// `Default` is written by hand rather than derived so that a caller
/// constructing a snapshot in code gets the same values as a caller decoding an
/// empty file. Deriving it would silently yield `0.0` for every geometric
/// field, and `normalize` would then clamp a correct width to zero.
impl Default for PersistedApp {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            unreadable: Unreadable::default(),
            active_project_id: None,
            active_agent_id: None,
            sidebar_visible: default_true(),
            sidebar_width: default_sidebar_width(),
            right_panel_visible: false,
            right_panel_width: default_panel_width(),
            min_right_panel_width: default_min_panel_width(),
            max_right_panel_width: default_max_panel_width(),
            right_panel_tab: RightPanelTab::default(),
            window_frame: None,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_sidebar_width() -> f32 {
    248.0
}
fn normalized_sidebar_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(200.0, 400.0)
    } else {
        default_sidebar_width()
    }
}
fn deserialize_sidebar_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f32, D::Error> {
    // A malformed layout preference must not discard the project list.
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(normalized_sidebar_width(
        value
            .as_f64()
            .map(|width| width as f32)
            .unwrap_or_else(default_sidebar_width),
    ))
}
fn default_panel_width() -> f32 {
    360.0
}
fn default_min_panel_width() -> f32 {
    280.0
}
fn default_max_panel_width() -> f32 {
    600.0
}

/// Kept for saved-workspace compatibility; the inspector only shows changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RightPanelTab {
    #[default]
    #[serde(alias = "files")]
    Changes,
}

/// One project's persisted state.
///
/// `remote = "Self"` for the same reason as `PersistedApp`: its thread,
/// workspace, and binding lists are decoded one element at a time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct PersistedProject {
    pub project: Option<Project>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    /// Threads, workspaces, and bindings this build could not read.
    #[serde(skip)]
    pub unreadable: Unreadable,
    /// Only chats created by FlowX are eligible. Removing an ID before a
    /// request is persisted ensures restart cannot repeat a paid title call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_title_pending: Vec<Uuid>,
    /// Archived threads retain their place, history, and provider binding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub archived_agent_ids: Vec<Uuid>,
    #[serde(default)]
    pub is_expanded: bool,
    #[serde(rename = "lastSelectedAgentId")]
    pub last_selected_agent_id: Option<Uuid>,
    #[serde(rename = "selectedInspectorPath")]
    pub selected_inspector_path: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<PersistedWorkspace>,
    #[serde(default)]
    pub native_bindings: Vec<PersistedBinding>,
    /// Local deletion hides a provider thread without deleting its external record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed_native_threads: Vec<PersistedNativeIdentity>,
    /// User choices take precedence over future provider metadata refreshes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread_overrides: Vec<PersistedThreadOverride>,
    /// Provider-deleted threads keep their local cache and presentation for a
    /// later reappearance, but no longer clutter the normal sidebar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_native_agent_ids: Vec<Uuid>,
    #[serde(rename = "nativePresentationIds", default)]
    pub native_presentation_ids: Vec<PersistedPresentation>,
    /// Threads whose turn finished while another thread was selected. The
    /// sidebar marks each one until it is opened, including after a relaunch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unseen_completion_ids: Vec<Uuid>,
}

/// Per-thread layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedWorkspace {
    #[serde(rename = "agentId")]
    pub agent_id: Uuid,
    #[serde(default)]
    pub split_open: bool,
    #[serde(default)]
    pub split_content: SplitContent,
    #[serde(rename = "splitRatio", default = "default_split_ratio")]
    pub split_ratio: f32,
    #[serde(default)]
    pub terminal_visible: bool,
    #[serde(rename = "terminalHeight", default = "default_terminal_height")]
    pub terminal_height: f32,
    #[serde(rename = "terminalCount", default = "default_terminal_count")]
    pub terminal_count: u32,
    #[serde(rename = "browserUrl", default)]
    pub browser_url: String,
    #[serde(rename = "scrollOffset", default)]
    pub scroll_offset: f32,
    #[serde(rename = "pinnedToBottom", default = "default_true")]
    pub pinned_to_bottom: bool,
}

fn default_split_ratio() -> f32 {
    0.5
}
fn default_terminal_height() -> f32 {
    220.0
}
fn default_terminal_count() -> u32 {
    1
}

impl Default for PersistedWorkspace {
    fn default() -> Self {
        Self {
            agent_id: Uuid::nil(),
            split_open: false,
            split_content: SplitContent::default(),
            split_ratio: default_split_ratio(),
            terminal_visible: false,
            terminal_height: default_terminal_height(),
            terminal_count: default_terminal_count(),
            browser_url: String::new(),
            scroll_offset: 0.0,
            pinned_to_bottom: true,
        }
    }
}

/// What the center split shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitContent {
    #[default]
    Diff,
    Browser,
}

/// A binding to a provider-native thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedBinding {
    #[serde(rename = "agentId")]
    pub agent_id: Uuid,
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "workingDirectory", default)]
    pub working_directory: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: Timestamp,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// A level this build does not know asks first. Unset is not narrower:
    /// it falls to the app default, which is the widest.
    #[serde(
        rename = "agentAccess",
        default,
        deserialize_with = "fx_core::models::agent::access_or_inherit"
    )]
    pub agent_access: Option<AgentAccess>,
    #[serde(rename = "isArchived", default)]
    pub is_archived: bool,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub context_window_size: Option<u64>,
    /// Do not replace a locally completed turn until native history contains it.
    #[serde(default)]
    pub local_history_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedNativeIdentity {
    pub provider_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedThreadOverride {
    pub agent_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

/// A remembered presentation for a provider thread that is not loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedPresentation {
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "agentId")]
    pub agent_id: Uuid,
}

impl PersistedApp {
    /// Clamp the stored geometry into its legal range.
    ///
    /// A file edited by hand, or written by a build with different bounds, must
    /// not be able to open the inspector off-screen.
    pub fn normalize(&mut self) {
        self.sidebar_width = normalized_sidebar_width(self.sidebar_width);
        // The bounds come from the same file. `f32::clamp` panics when they are
        // inverted, which made one bad pair crash every launch: the file parses,
        // so the backup is never tried.
        let (min, max) = (self.min_right_panel_width, self.max_right_panel_width);
        if !(min.is_finite() && max.is_finite() && 0.0 <= min && min <= max) {
            self.min_right_panel_width = default_min_panel_width();
            self.max_right_panel_width = default_max_panel_width();
        }
        if !self.right_panel_width.is_finite() {
            self.right_panel_width = default_panel_width();
        }
        self.right_panel_width = self
            .right_panel_width
            .clamp(self.min_right_panel_width, self.max_right_panel_width);
        for record in &mut self.projects {
            let agents: std::collections::HashSet<_> =
                record.agents.iter().map(|agent| agent.id).collect();
            // A thread this build cannot read is still a thread: it is carried
            // to the next save, and so is what the project says about it. Pruned,
            // a newer build's archived thread came back unarchived.
            let carried: std::collections::HashSet<Uuid> = record
                .unreadable
                .elements("agents")
                .iter()
                .filter_map(|agent| agent.get("id")?.as_str()?.parse().ok())
                .collect();
            let known = |id: &Uuid| agents.contains(id) || carried.contains(id);
            let mut seen = std::collections::HashSet::new();
            record
                .auto_title_pending
                .retain(|id| known(id) && seen.insert(*id));
            seen.clear();
            record
                .archived_agent_ids
                .retain(|id| known(id) && seen.insert(*id));
            if record
                .last_selected_agent_id
                .is_some_and(|id| !agents.contains(&id) || record.archived_agent_ids.contains(&id))
            {
                record.last_selected_agent_id = None;
            }
            // Native indexes can contain thousands of threads. File byte limits
            // bound input; normalization must never silently discard history.
            for workspace in &mut record.workspaces {
                workspace.split_ratio = workspace.split_ratio.clamp(0.2, 0.8);
                workspace.terminal_height = workspace.terminal_height.clamp(120.0, 500.0);
                workspace.terminal_count = workspace.terminal_count.clamp(1, 3);
                workspace.scroll_offset = workspace.scroll_offset.max(0.0);
                // The old default was an https URL that was never a real page.
                if workspace.browser_url == "https://localhost:3000" {
                    workspace.browser_url.clear();
                }
            }
            if record.native_presentation_ids.len() > MAX_DORMANT_PRESENTATIONS {
                record
                    .native_presentation_ids
                    .truncate(MAX_DORMANT_PRESENTATIONS);
            }
        }
    }

    /// Drop projects whose path is not usable.
    ///
    /// Corrupt entries are removed rather than repaired, because there is no
    /// path to recover: a project is identified by where it lives.
    pub fn drop_unusable_projects(&mut self) {
        let canonical: Vec<String> = self
            .projects
            .iter()
            .filter_map(|p| {
                p.project
                    .as_ref()
                    .and_then(|p| p.canonical_root_path().to_str().map(str::to_string))
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::new();
        for mut project in self.projects.drain(..) {
            let Some(inner) = project.project.as_mut() else {
                continue;
            };
            if fx_core::models::project::validated_persisted_root_path(&inner.root_path).is_none() {
                continue;
            }
            let key = inner.canonical_root_path().to_string_lossy().into_owned();
            if !seen.insert(key) {
                continue;
            }
            // Threads not in the order list are appended, so an incomplete list
            // reorders nothing.
            let listed: std::collections::HashSet<Uuid> =
                inner.agent_order.iter().copied().collect();
            for agent in &project.agents {
                if !listed.contains(&agent.id) {
                    inner.agent_order.push(agent.id);
                }
            }
            kept.push(project);
        }
        let _ = canonical;
        self.projects = kept;
    }
}

impl PersistedUnsentPrompt {
    pub fn new(text: String, attachment_count: usize, session_id: Option<String>) -> Self {
        Self {
            text,
            attachment_count,
            session_id,
        }
    }
}

// ── Conversations ──────────────────────────────────────────────────────────

/// One thread's persisted transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConversation {
    #[serde(rename = "agentId")]
    pub agent_id: Uuid,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<ConversationMessage>,
    #[serde(rename = "totalCostUsd", default)]
    pub total_cost_usd: f64,
    #[serde(rename = "totalInputTokens", default)]
    pub total_input_tokens: u64,
    #[serde(rename = "totalOutputTokens", default)]
    pub total_output_tokens: u64,
    #[serde(rename = "totalCachedInputTokens", default)]
    pub total_cached_input_tokens: u64,
    #[serde(rename = "totalReasoningOutputTokens", default)]
    pub total_reasoning_output_tokens: u64,
    #[serde(rename = "totalTokens", default)]
    pub total_tokens: u64,
    #[serde(rename = "currentContextTokens", default)]
    pub current_context_tokens: u64,
    #[serde(rename = "reportedContextWindow", default)]
    pub reported_context_window: Option<u64>,
    /// A draft that failed to send, kept so the user can retry it.
    #[serde(rename = "unsentPrompt", default)]
    pub unsent_prompt: Option<PersistedUnsentPrompt>,
    /// Text still in the composer, independent of a submitted prompt awaiting acknowledgment.
    #[serde(rename = "draftText", default)]
    pub draft_text: String,
    /// The thread's goal as the provider last reported it, so the row above
    /// the composer is there before the provider is asked again.
    ///
    /// A goal this build cannot read costs only the goal, never the transcript.
    #[serde(
        default,
        deserialize_with = "deserialize_or_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: Option<fx_core::models::ConversationGoal>,
}

/// A prompt that failed to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedUnsentPrompt {
    pub text: String,
    #[serde(rename = "attachmentCount", default)]
    pub attachment_count: usize,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
}

impl PersistedConversation {
    /// Whether this thread is worth writing at all.
    ///
    /// An unopened provider thread has nothing to save. Writing it anyway would
    /// truncate every other thread's cache and prune its images on quit.
    pub fn is_worth_persisting(&self) -> bool {
        !self.messages.is_empty()
            || self.unsent_prompt.is_some()
            || !self.draft_text.is_empty()
            || self.session_id.is_some()
            || self.total_tokens > 0
            || self.goal.is_some()
    }

    /// Build a persisted record from a thread's messages.
    ///
    /// Takes the messages rather than a conversation type because this crate
    /// must not depend on `fx-agent`: the on-disk format belongs to the domain,
    /// and the engine that produces those messages is the app's problem, not the
    /// format's.
    pub fn from_messages(agent_id: Uuid, messages: Vec<ConversationMessage>) -> Self {
        Self {
            agent_id,
            session_id: None,
            messages,
            total_cost_usd: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cached_input_tokens: 0,
            total_reasoning_output_tokens: 0,
            total_tokens: 0,
            current_context_tokens: 0,
            reported_context_window: None,
            unsent_prompt: None,
            draft_text: String::new(),
            goal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::MessageRole;

    fn workspace() -> PersistedWorkspace {
        PersistedWorkspace::default()
    }

    #[test]
    fn preferences_from_before_provider_defaults_keep_their_appearance() {
        let legacy: PersistedPreferences = serde_json::from_str(
            r#"{"appearance":"light","base_tone":"stone","accent":"blue","text_size":"large"}"#,
        )
        .unwrap();
        assert_eq!(legacy.appearance, "light");
        assert_eq!(legacy.base_tone, "stone");
        assert_eq!(legacy.accent, "blue");
        assert_eq!(legacy.text_size, "large");
        assert_eq!(legacy.codex_defaults, ProviderDefaults::default());
        assert_eq!(legacy.claude_defaults, ProviderDefaults::default());
        assert_eq!(legacy.auto_titles, AutoTitlePreferences::default());
        let project: PersistedProject = serde_json::from_str("{}").unwrap();
        assert!(project.auto_title_pending.is_empty());
    }

    #[test]
    fn malformed_provider_choices_do_not_discard_other_preferences() {
        let decoded: PersistedPreferences = serde_json::from_str(
            r#"{"appearance":"light","codex_defaults":{"model_id":"saved-model","effort":42,"agent_access":"futureAccess"},"claude_defaults":null}"#,
        )
        .unwrap();
        assert_eq!(decoded.appearance, "light");
        assert_eq!(
            decoded.codex_defaults.model_id.as_deref(),
            Some("saved-model")
        );
        assert_eq!(decoded.codex_defaults.effort, None);
        assert_eq!(
            decoded.codex_defaults.agent_access,
            Some(AgentAccess::Supervised),
            "an unknown level asks first rather than falling to Full Access"
        );
        assert_eq!(decoded.claude_defaults, ProviderDefaults::default());
    }

    #[test]
    fn legacy_title_models_migrate_to_one_shared_codex_choice() {
        let preferences: PersistedPreferences = serde_json::from_str(
            r#"{"auto_titles":{"enabled":false,"codex_model_id":"custom-small-model","claude_model_id":"haiku"}}"#,
        ).unwrap();
        assert_eq!(
            preferences.auto_titles,
            AutoTitlePreferences {
                enabled: false,
                provider_id: "codex".into(),
                model_id: "custom-small-model".into(),
            }
        );
        let written = serde_json::to_value(&preferences.auto_titles).unwrap();
        assert!(written.get("codex_model_id").is_none());
        assert!(written.get("claude_model_id").is_none());
        assert_eq!(written["provider_id"], "codex");
        assert_eq!(written["model_id"], "custom-small-model");
    }

    #[test]
    fn shared_title_preferences_round_trip_and_recover_fields_independently() {
        let preferences = AutoTitlePreferences {
            enabled: false,
            provider_id: "claude".into(),
            model_id: "custom-haiku".into(),
        };
        let written = serde_json::to_string(&preferences).unwrap();
        assert_eq!(
            serde_json::from_str::<AutoTitlePreferences>(&written).unwrap(),
            preferences
        );
        let malformed: AutoTitlePreferences =
            serde_json::from_str(r#"{"enabled":false,"provider_id":"claude","model_id":42}"#)
                .unwrap();
        assert!(!malformed.enabled);
        assert_eq!(malformed.provider_id, "claude");
        assert_eq!(malformed.model_id, "haiku");
        let unknown: AutoTitlePreferences =
            serde_json::from_str(r#"{"provider_id":"future-provider","model_id":"foreign-model"}"#)
                .unwrap();
        assert_eq!(unknown, AutoTitlePreferences::default());
        let invalid: AutoTitlePreferences =
            serde_json::from_str(r#"{"provider_id":42,"model_id":"foreign-model"}"#).unwrap();
        assert_eq!(invalid, AutoTitlePreferences::default());
    }

    #[test]
    fn constructing_and_decoding_agree_on_defaults() {
        // The two paths share one source of truth; this fails the moment they
        // drift.
        let decoded: PersistedApp = serde_json::from_str("{}").unwrap();
        let built = PersistedApp::default();
        assert_eq!(decoded.right_panel_width, built.right_panel_width);
        assert_eq!(decoded.min_right_panel_width, built.min_right_panel_width);
        assert_eq!(decoded.max_right_panel_width, built.max_right_panel_width);
        assert_eq!(decoded.sidebar_visible, built.sidebar_visible);
        assert_eq!(decoded.sidebar_width, built.sidebar_width);
        assert_eq!(decoded.right_panel_visible, built.right_panel_visible);
        assert_eq!(decoded.right_panel_tab, built.right_panel_tab);
    }

    #[test]
    fn an_unknown_inspector_tab_reads_as_the_default_and_keeps_the_projects() {
        let agent = Agent::new("Kept");
        let mut value = serde_json::to_value(PersistedApp {
            projects: vec![PersistedProject {
                agents: vec![agent.clone()],
                ..Default::default()
            }],
            ..Default::default()
        })
        .unwrap();
        value["rightPanelTab"] = serde_json::json!("terminal");
        let decoded: PersistedApp = serde_json::from_value(value).expect("the snapshot reads");
        assert_eq!(decoded.right_panel_tab, RightPanelTab::Changes);
        assert_eq!(decoded.projects.len(), 1);
        assert_eq!(decoded.projects[0].agents, vec![agent]);
    }

    #[test]
    fn an_unreadable_threads_archive_and_title_flags_survive_normalizing() {
        let readable = Agent::new("Readable");
        let unreadable = Uuid::new_v4();
        let mut value = serde_json::to_value(PersistedProject {
            agents: vec![readable.clone()],
            archived_agent_ids: vec![readable.id, unreadable],
            auto_title_pending: vec![unreadable],
            ..Default::default()
        })
        .unwrap();
        // A thread written by a newer build, in a shape this one cannot read.
        value["agents"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "id": unreadable.to_string(), "kind": "future" }));
        let project: PersistedProject = serde_json::from_value(value).unwrap();
        assert_eq!(project.unreadable.elements("agents").len(), 1);
        let mut app = PersistedApp {
            projects: vec![project],
            ..Default::default()
        };
        app.normalize();
        assert_eq!(
            app.projects[0].archived_agent_ids,
            vec![readable.id, unreadable]
        );
        assert_eq!(app.projects[0].auto_title_pending, vec![unreadable]);
    }

    #[test]
    fn an_unreadable_goal_costs_only_the_goal() {
        let mut value = serde_json::to_value(PersistedConversation::from_messages(
            Uuid::new_v4(),
            vec![ConversationMessage::text(MessageRole::User, "kept")],
        ))
        .unwrap();
        value["goal"] = serde_json::json!({ "threadId": "t", "objective": 7 });
        let decoded: PersistedConversation =
            serde_json::from_value(value).expect("the transcript reads");
        assert_eq!(decoded.messages.len(), 1);
        assert_eq!(decoded.goal, None);
    }

    #[test]
    fn archive_metadata_defaults_safely_and_keeps_only_distinct_existing_threads() {
        let legacy: PersistedProject = serde_json::from_str("{}").unwrap();
        assert!(legacy.archived_agent_ids.is_empty());
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("archived_agent_ids")
                .is_none()
        );
        let agent = Agent::new("Archived thread");
        let mut app = PersistedApp {
            projects: vec![PersistedProject {
                agents: vec![agent.clone()],
                archived_agent_ids: vec![agent.id, Uuid::new_v4(), agent.id],
                last_selected_agent_id: Some(agent.id),
                ..Default::default()
            }],
            ..Default::default()
        };
        app.normalize();
        assert_eq!(app.projects[0].archived_agent_ids, vec![agent.id]);
        assert_eq!(app.projects[0].last_selected_agent_id, None);
        let restored: PersistedApp =
            serde_json::from_value(serde_json::to_value(app).unwrap()).unwrap();
        assert_eq!(restored.projects[0].archived_agent_ids, vec![agent.id]);
    }

    #[test]
    fn a_record_without_a_domain_project_still_normalizes_its_layout() {
        // Losing the project must not also lose the user's layout.
        let mut w = workspace();
        w.browser_url = "https://localhost:3000".into();
        w.split_ratio = 9.0;
        let mut app = PersistedApp {
            projects: vec![PersistedProject {
                project: None,
                workspaces: vec![w],
                ..Default::default()
            }],
            ..Default::default()
        };
        app.normalize();
        let w = &app.projects[0].workspaces[0];
        assert_eq!(w.browser_url, "");
        assert_eq!(w.split_ratio, 0.8);
    }

    #[test]
    fn an_unchecked_snapshot_declares_legal_geometry() {
        let mut app = PersistedApp {
            right_panel_width: 9000.0,
            ..Default::default()
        };
        app.normalize();
        assert_eq!(app.right_panel_width, app.max_right_panel_width);
    }

    #[test]
    fn a_too_narrow_panel_is_widened_to_the_minimum() {
        let mut app = PersistedApp {
            right_panel_width: 10.0,
            ..Default::default()
        };
        app.normalize();
        assert_eq!(app.right_panel_width, app.min_right_panel_width);
    }

    #[test]
    fn inverted_or_infinite_panel_bounds_are_repaired_instead_of_panicking() {
        for (min, max, width) in [
            (700.0, 600.0, 650.0),
            (f32::INFINITY, f32::INFINITY, 420.0),
            (-50.0, 600.0, 420.0),
            (300.0, 600.0, f32::INFINITY),
        ] {
            let mut app = PersistedApp {
                min_right_panel_width: min,
                max_right_panel_width: max,
                right_panel_width: width,
                ..Default::default()
            };
            app.normalize();
            assert!(app.min_right_panel_width <= app.max_right_panel_width);
            assert!(app.right_panel_width.is_finite(), "{min} {max} {width}");
            assert!(
                (app.min_right_panel_width..=app.max_right_panel_width)
                    .contains(&app.right_panel_width)
            );
        }

        // The same pair decoded from a file, as a launch reads it.
        let mut app: PersistedApp = serde_json::from_str(
            r#"{"projects":[],"minRightPanelWidth":700,"maxRightPanelWidth":600,"rightPanelWidth":1e39}"#,
        )
        .unwrap();
        app.normalize();
        assert_eq!(app.min_right_panel_width, default_min_panel_width());
        assert_eq!(app.max_right_panel_width, default_max_panel_width());
    }

    #[test]
    fn a_valid_geometry_is_left_alone() {
        let mut app = PersistedApp {
            right_panel_width: 420.0,
            ..Default::default()
        };
        app.normalize();
        assert_eq!(app.right_panel_width, 420.0);
    }

    #[test]
    fn sidebar_geometry_repairs_invalid_preferences_without_losing_other_settings() {
        for (value, expected) in [
            ("-10", 200.0),
            ("10000", 400.0),
            ("320.5", 320.5),
            ("1e100", 248.0),
            ("null", 248.0),
            ("\"broken\"", 248.0),
            ("{}", 248.0),
        ] {
            let json = format!(
                r#"{{"sidebarWidth":{value},"sidebar_visible":false,"rightPanelTab":"files"}}"#
            );
            let app: PersistedApp = serde_json::from_str(&json).unwrap();
            assert_eq!(app.sidebar_width, expected, "value: {value}");
            assert!(!app.sidebar_visible);
            assert_eq!(app.right_panel_tab, RightPanelTab::Changes);
        }
        for width in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut app = PersistedApp {
                sidebar_width: width,
                ..Default::default()
            };
            app.normalize();
            assert_eq!(app.sidebar_width, 248.0);
        }
    }

    #[test]
    fn a_custom_sidebar_width_round_trips_under_its_stable_key() {
        let app = PersistedApp {
            sidebar_width: 320.5,
            sidebar_visible: false,
            ..Default::default()
        };
        let value = serde_json::to_value(app).unwrap();
        assert_eq!(value["sidebarWidth"], 320.5);
        let restored: PersistedApp = serde_json::from_value(value).unwrap();
        assert_eq!(restored.sidebar_width, 320.5);
        assert!(!restored.sidebar_visible);
    }

    #[test]
    fn workspace_geometry_is_clamped() {
        let mut project = PersistedProject::default();
        let mut w = workspace();
        w.split_ratio = 4.0;
        w.terminal_height = 99_999.0;
        w.terminal_count = 9;
        w.scroll_offset = -50.0;
        project.workspaces.push(w);
        let mut app = PersistedApp {
            projects: vec![project],
            ..Default::default()
        };
        app.normalize();

        let w = &app.projects[0].workspaces[0];
        assert_eq!(w.split_ratio, 0.8);
        assert_eq!(w.terminal_height, 500.0);
        assert_eq!(w.terminal_count, 3);
        assert_eq!(w.scroll_offset, 0.0);
    }

    #[test]
    fn the_legacy_placeholder_browser_url_is_cleared() {
        // It was a default that was never a real page, so leaving it would put
        // a useless "address" in every restored workspace.
        let mut project = PersistedProject::default();
        let mut w = workspace();
        w.browser_url = "https://localhost:3000".into();
        project.workspaces.push(w);
        let mut app = PersistedApp {
            projects: vec![project],
            ..Default::default()
        };
        app.normalize();
        assert_eq!(app.projects[0].workspaces[0].browser_url, "");
    }

    #[test]
    fn a_real_browser_url_is_preserved() {
        let mut project = PersistedProject::default();
        let mut w = workspace();
        w.browser_url = "http://localhost:5173".into();
        project.workspaces.push(w);
        let mut app = PersistedApp {
            projects: vec![project],
            ..Default::default()
        };
        app.normalize();
        assert_eq!(
            app.projects[0].workspaces[0].browser_url,
            "http://localhost:5173"
        );
    }

    #[test]
    fn a_project_without_a_path_is_dropped() {
        let mut app = PersistedApp {
            projects: vec![PersistedProject::default()],
            ..Default::default()
        };
        app.drop_unusable_projects();
        assert!(app.projects.is_empty());
    }

    #[test]
    fn a_relative_project_path_is_dropped() {
        let mut app = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(Project::new("relative/path")),
                ..Default::default()
            }],
            ..Default::default()
        };
        app.drop_unusable_projects();
        assert!(app.projects.is_empty());
    }

    #[test]
    fn two_spellings_of_one_directory_collapse_to_one_project() {
        let real = tempfile::tempdir().unwrap();
        let path = real.path().to_string_lossy().into_owned();

        let mut app = PersistedApp {
            projects: vec![
                PersistedProject {
                    project: Some(Project::new(&path)),
                    ..Default::default()
                },
                PersistedProject {
                    project: Some(Project::new(format!("{path}/./"))),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        app.drop_unusable_projects();
        assert_eq!(app.projects.len(), 1, "the same directory opened twice");
    }

    #[test]
    fn a_thread_missing_from_the_order_is_appended_not_reordered() {
        // An incomplete order list must not reshuffle the sidebar.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        let mut project = Project::new(&path);
        let listed = Uuid::from_u128(1);
        let unlisted = Uuid::from_u128(2);
        project.agent_order = vec![listed];

        let mut app = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(project),
                agents: vec![
                    Agent::with_id(listed, "first"),
                    Agent::with_id(unlisted, "second"),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        app.drop_unusable_projects();

        let order = &app.projects[0].project.as_ref().unwrap().agent_order;
        assert_eq!(
            order,
            &[listed, unlisted],
            "the existing order was disturbed"
        );
    }

    #[test]
    fn native_order_lists_keep_every_thread() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        let mut project = Project::new(&path);
        project.agent_order = (0..2_000).map(|i| Uuid::from_u128(i as u128)).collect();

        let mut app = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(project),
                ..Default::default()
            }],
            ..Default::default()
        };
        app.normalize();
        assert_eq!(
            app.projects[0].project.as_ref().unwrap().agent_order.len(),
            2_000
        );
    }

    #[test]
    fn an_empty_transcript_is_not_worth_writing() {
        // Writing it would truncate every other thread on quit.
        let persisted = PersistedConversation::from_messages(Uuid::nil(), Vec::new());
        assert!(!persisted.is_worth_persisting());
    }

    #[test]
    fn a_transcript_with_messages_is_worth_writing() {
        let persisted = PersistedConversation::from_messages(
            Uuid::nil(),
            vec![ConversationMessage::text(MessageRole::User, "hi")],
        );
        assert!(persisted.is_worth_persisting());
    }

    #[test]
    fn a_goal_survives_a_round_trip_and_old_files_have_none() {
        let goal = fx_core::models::ConversationGoal::from_codex(&serde_json::json!({
            "threadId": "t", "objective": "Ship it", "status": "active"
        }))
        .unwrap();
        let persisted = PersistedConversation {
            goal: Some(goal.clone()),
            ..PersistedConversation::from_messages(Uuid::nil(), Vec::new())
        };
        let bytes = serde_json::to_vec(&persisted).unwrap();
        let read: PersistedConversation = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(read.goal, Some(goal));
        let legacy: PersistedConversation =
            serde_json::from_str(r#"{"agentId":"00000000-0000-0000-0000-000000000000"}"#).unwrap();
        assert_eq!(legacy.goal, None);
        let preferences: PersistedPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(preferences.follow_up_mode, "steer");
    }

    #[test]
    fn an_unsent_draft_alone_makes_a_transcript_worth_writing() {
        let persisted = PersistedConversation {
            unsent_prompt: Some(PersistedUnsentPrompt::new("draft".into(), 1, None)),
            ..PersistedConversation::from_messages(Uuid::nil(), Vec::new())
        };
        assert!(
            persisted.is_worth_persisting(),
            "a user's unsent draft was discarded on quit"
        );
    }

    #[test]
    fn an_app_snapshot_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_string_lossy().into_owned();
        let app = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(Project::new(&path)),
                workspaces: vec![PersistedWorkspace {
                    browser_url: "http://localhost:3000".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            active_project_id: Some(Uuid::from_u128(1)),
            right_panel_visible: true,
            right_panel_tab: RightPanelTab::Changes,
            ..Default::default()
        };
        let json = serde_json::to_string(&app).unwrap();
        let back: PersistedApp = serde_json::from_str(&json).unwrap();
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.right_panel_tab, RightPanelTab::Changes);
        assert_eq!(
            back.projects[0].workspaces[0].browser_url,
            "http://localhost:3000"
        );
    }

    #[test]
    fn a_payload_missing_every_optional_field_still_decodes() {
        // A file from an earlier build must still open.
        let json = r#"{"projects":[],"activeProjectId":null,"activeAgentId":null}"#;
        let app: PersistedApp = serde_json::from_str(json).unwrap();
        assert!(app.projects.is_empty());
        assert!(app.sidebar_visible, "the default should be visible");
        assert_eq!(app.sidebar_width, 248.0);
        assert!(!app.right_panel_visible);
    }

    /// A project file as a build with values this one does not know wrote it.
    fn file_with_entries_this_build_cannot_read(root: &str) -> serde_json::Value {
        let agent = Uuid::from_u128(1);
        serde_json::json!({
            "projects": [
                {
                    "project": serde_json::to_value(Project::new(root)).unwrap(),
                    "agents": [
                        {"id": agent, "title": "Readable",
                         "created_at": "2026-01-01T00:00:00Z",
                         "execution_state": "cancelled",
                         "configuration": {"agent_access": "readOnly"}},
                        {"id": "not-a-uuid", "title": "From a newer build",
                         "created_at": "2026-01-01T00:00:00Z"}
                    ],
                    "workspaces": [
                        {"agentId": agent},
                        {"agentId": agent, "terminalCount": "three"}
                    ],
                    "native_bindings": [
                        {"agentId": agent, "providerId": "codex", "sessionId": "s1",
                         "updatedAt": "2026-01-01T00:00:00Z", "agentAccess": "readOnly"},
                        {"agentId": agent, "providerId": "codex"}
                    ]
                },
                {"project": {"id": 7, "rootPath": ["not", "a", "path"]}}
            ],
            "activeProjectId": null,
            "activeAgentId": null
        })
    }

    #[test]
    fn values_from_a_newer_build_cost_only_the_entries_that_hold_them() {
        // One unknown value used to fail the whole file, and the backup holds
        // the same bytes, so every project disappeared.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().into_owned();
        let app: PersistedApp =
            serde_json::from_value(file_with_entries_this_build_cannot_read(&root)).unwrap();

        assert_eq!(app.projects.len(), 1);
        assert_eq!(app.unreadable.elements("projects").len(), 1);
        let record = &app.projects[0];
        assert_eq!(record.agents.len(), 1);
        assert_eq!(
            record.agents[0].execution_state,
            fx_core::AgentExecutionState::Idle
        );
        // An access level this build cannot read asks first: unset would
        // fall to the app default, which is Full Access.
        assert_eq!(
            record.agents[0].configuration.agent_access,
            Some(AgentAccess::Supervised)
        );
        assert_eq!(record.workspaces.len(), 1);
        assert_eq!(record.native_bindings.len(), 1);
        assert_eq!(
            record.native_bindings[0].agent_access,
            Some(AgentAccess::Supervised)
        );
        for key in ["agents", "workspaces", "native_bindings"] {
            assert_eq!(record.unreadable.elements(key).len(), 1, "{key}");
        }
    }

    #[test]
    fn an_entry_this_build_cannot_read_is_written_back_unchanged() {
        // Skipping it and saving would delete it for the build that wrote it.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().into_owned();
        let original = file_with_entries_this_build_cannot_read(&root);
        let mut app: PersistedApp = serde_json::from_value(original.clone()).unwrap();
        app.projects[0].agents[0].title = "Renamed here".into();

        let written = serde_json::to_value(&app).unwrap();
        assert_eq!(written["projects"][1], original["projects"][1]);
        let project = &written["projects"][0];
        assert_eq!(project["agents"][0]["title"], "Renamed here");
        for key in ["agents", "workspaces", "native_bindings"] {
            assert_eq!(project[key][1], original["projects"][0][key][1], "{key}");
        }

        // And again, as a second save reads the first one back.
        let reread: PersistedApp = serde_json::from_value(written.clone()).unwrap();
        assert_eq!(serde_json::to_value(&reread).unwrap(), written);
    }

    #[test]
    fn a_list_that_is_not_a_list_still_fails_its_record() {
        // There is no element to set aside, so refusing is the only way to keep
        // the value: the record is then kept whole by the list around it.
        let decoded: PersistedApp =
            serde_json::from_str(r#"{"projects":[{"agents":{"oops":true}}]}"#).unwrap();
        assert!(decoded.projects.is_empty());
        assert_eq!(
            decoded.unreadable.elements("projects"),
            &[serde_json::json!({"agents": {"oops": true}})]
        );
        assert!(serde_json::from_str::<PersistedApp>(r#"{"projects":{}}"#).is_err());
    }

    #[test]
    fn a_conversation_with_an_unknown_message_type_fails_loudly() {
        // Unlike a project list, a single unparseable content block has no safe
        // partial reading, so this must not silently drop content.
        let json = r#"{
            "agentId":"00000000-0000-0000-0000-000000000000",
            "messages":[{"id":"00000000-0000-0000-0000-000000000001",
                         "role":"user","timestamp":"2026-01-01T00:00:00Z",
                         "content":[{"type":"telepathy","text":"hi"}]}]
        }"#;
        assert!(serde_json::from_str::<PersistedConversation>(json).is_err());
    }
}
