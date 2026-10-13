//! The on-disk shape.
//!
//! Deliberately not a mirror of the in-memory types. The persisted form is
//! allowed to be flatter, to carry migration fields, and to validate what it
//! reads — the in-memory model should not have to know any of that.
//!
//! The rule this file follows throughout: **decoding must never fail because of
//! one bad value.** A project whose thread list is malformed loses that thread,
//! not the whole file.

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
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_or_default"
    )]
    pub agent_access: Option<AgentAccess>,
}

/// Separate lightweight models used once to name a newly created chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoTitlePreferences {
    pub enabled: bool,
    pub codex_model_id: String,
    pub claude_model_id: String,
}

impl Default for AutoTitlePreferences {
    fn default() -> Self {
        Self {
            enabled: true,
            codex_model_id: "gpt-5.6-luna".into(),
            claude_model_id: "haiku".into(),
        }
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
        }
    }
}

// ── Projects ───────────────────────────────────────────────────────────────

/// The project and layout snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedApp {
    #[serde(default)]
    pub projects: Vec<PersistedProject>,
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
    #[serde(rename = "rightPanelTab", default)]
    pub right_panel_tab: RightPanelTab,
}

/// `Default` is written by hand rather than derived so that a caller
/// constructing a snapshot in code gets the same values as a caller decoding an
/// empty file. Deriving it would silently yield `0.0` for every geometric
/// field, and `normalize` would then clamp a correct width to zero.
impl Default for PersistedApp {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            active_project_id: None,
            active_agent_id: None,
            sidebar_visible: default_true(),
            sidebar_width: default_sidebar_width(),
            right_panel_visible: false,
            right_panel_width: default_panel_width(),
            min_right_panel_width: default_min_panel_width(),
            max_right_panel_width: default_max_panel_width(),
            right_panel_tab: RightPanelTab::default(),
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

/// Which tab the inspector shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RightPanelTab {
    #[default]
    Changes,
    Files,
}

/// One project's persisted state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedProject {
    pub project: Option<Project>,
    #[serde(default)]
    pub agents: Vec<Agent>,
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
}

/// Per-thread layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(rename = "agentAccess", default)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let (min, max) = (self.min_right_panel_width, self.max_right_panel_width);
        self.right_panel_width = self.right_panel_width.clamp(min, max);
        for record in &mut self.projects {
            let agents: std::collections::HashSet<_> =
                record.agents.iter().map(|agent| agent.id).collect();
            let mut seen = std::collections::HashSet::new();
            record
                .auto_title_pending
                .retain(|id| agents.contains(id) && seen.insert(*id));
            seen.clear();
            record
                .archived_agent_ids
                .retain(|id| agents.contains(id) && seen.insert(*id));
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
        assert_eq!(decoded.codex_defaults.agent_access, None);
        assert_eq!(decoded.claude_defaults, ProviderDefaults::default());
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
            assert_eq!(app.right_panel_tab, RightPanelTab::Files);
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
            right_panel_tab: RightPanelTab::Files,
            ..Default::default()
        };
        let json = serde_json::to_string(&app).unwrap();
        let back: PersistedApp = serde_json::from_str(&json).unwrap();
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.right_panel_tab, RightPanelTab::Files);
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
