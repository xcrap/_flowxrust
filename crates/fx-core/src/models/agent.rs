use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Timestamp, now};

/// One conversation thread inside a project.
///
/// FlowX calls these "agents" internally, but a row in the sidebar is almost
/// always a provider-native thread rather than something FlowX owns, so
/// "thread" is the honest word. The struct keeps the original name to stay
/// recognizable against the Swift source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub id: Uuid,
    pub title: String,
    #[serde(default)]
    pub configuration: AgentConfiguration,
    #[serde(default)]
    pub execution_state: AgentExecutionState,
    pub created_at: Timestamp,
}

impl Agent {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            configuration: AgentConfiguration::default(),
            execution_state: AgentExecutionState::Idle,
            created_at: now(),
        }
    }

    pub fn with_id(id: Uuid, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            configuration: AgentConfiguration::default(),
            execution_state: AgentExecutionState::Idle,
            created_at: now(),
        }
    }
}

/// Where a thread is in its lifecycle.
///
/// This is derived state kept alongside the thread so the sidebar can render
/// without consulting the conversation engine, which may be mid-flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentExecutionState {
    #[default]
    Idle,
    Running,
    Success,
    Failure,
    WaitingForApproval,
}

impl AgentExecutionState {
    pub const ALL: [Self; 5] = [
        Self::Idle,
        Self::Running,
        Self::Success,
        Self::Failure,
        Self::WaitingForApproval,
    ];
}

/// Whether the agent works freely or proposes a plan first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMode {
    #[default]
    Auto,
    Plan,
}

impl AgentMode {
    pub const ALL: [Self; 2] = [Self::Auto, Self::Plan];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Chat",
            Self::Plan => "Plan",
        }
    }

    /// The one-line explanation shown as the dropdown subtitle.
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Auto => "Allow the agent to work normally",
            Self::Plan => "Ask for a plan before making changes",
        }
    }
}

/// How much the agent may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentAccess {
    Supervised,
    AcceptEdits,
    #[default]
    FullAccess,
}

impl AgentAccess {
    pub const ALL: [Self; 3] = [Self::Supervised, Self::AcceptEdits, Self::FullAccess];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Supervised => "Supervised",
            Self::AcceptEdits => "Accept Edits",
            Self::FullAccess => "Full Access",
        }
    }

    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Supervised => "Ask before commands and edits",
            Self::AcceptEdits => "Allow edits and review other actions",
            // Questions always require an answer regardless of access level,
            // which is why this label says so.
            Self::FullAccess => "Auto-approve tools; questions still appear",
        }
    }
}

/// A thread's overrides against the app defaults and the provider's own
/// policy.
///
/// Every field is optional on purpose. `None` means *inherit*, which is
/// meaningfully different from picking a value: a provider-native thread that
/// leaves mode and access unset stays under the provider's persisted policy,
/// where an inferred value would turn inheritance into an override.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<AgentMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_access: Option<AgentAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
}

impl AgentConfiguration {
    /// The effective mode, inheriting the app default when unset.
    pub fn resolved_mode(&self) -> AgentMode {
        self.agent_mode.unwrap_or_default()
    }

    /// The effective access level, inheriting the app default when unset.
    pub fn resolved_access(&self, app_default: AgentAccess) -> AgentAccess {
        self.agent_access.unwrap_or(app_default)
    }

    /// Whether this configuration states any mode or access override at all.
    ///
    /// `None` means *inherit*, which is meaningfully different from picking a
    /// value: a provider-native thread that leaves both unset stays under the
    /// provider's own persisted policy. Persisting an inferred value here would
    /// silently turn inheritance into an override on the next load, so the
    /// accessors below never do it.
    pub fn has_mode_or_access_override(&self) -> bool {
        self.agent_mode.is_some() || self.agent_access.is_some()
    }

    /// Build the effective configuration by layering explicit overrides over
    /// the values already resolved from the provider or the app defaults.
    ///
    /// `app_defaults` supplies provider, model, effort, and access; `native`
    /// supplies what the provider's own thread recorded. An explicit override
    /// wins over both.
    pub fn resolve(
        &self,
        native: &NativeDefaults,
        app_defaults: &AppDefaults,
    ) -> ResolvedConfiguration {
        ResolvedConfiguration {
            provider_id: self
                .provider_id
                .clone()
                .or_else(|| native.provider_id.clone())
                .unwrap_or_else(|| app_defaults.provider_id.clone()),
            model_id: self
                .model_id
                .clone()
                .or_else(|| native.model_id.clone())
                .unwrap_or_else(|| app_defaults.model_id.clone()),
            effort: self
                .effort
                .clone()
                .or_else(|| native.effort.clone())
                .unwrap_or_else(|| app_defaults.effort.clone()),
            agent_mode: self.agent_mode.or(native.agent_mode).unwrap_or_default(),
            agent_access: self
                .agent_access
                .or(native.agent_access)
                .unwrap_or(app_defaults.agent_access),
            system_prompt: self.system_prompt.clone(),
            context_window_size: self.context_window_size.or(native.context_window_size),
        }
    }
}

/// What a provider's own thread recorded about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeDefaults {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub effort: Option<String>,
    pub agent_mode: Option<AgentMode>,
    pub agent_access: Option<AgentAccess>,
    pub context_window_size: Option<u64>,
}

/// What the app falls back to when neither the thread nor its provider says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDefaults {
    pub provider_id: String,
    pub model_id: String,
    pub effort: String,
    pub agent_access: AgentAccess,
}

impl Default for AppDefaults {
    fn default() -> Self {
        Self {
            provider_id: DEFAULT_PROVIDER_ID.to_string(),
            model_id: DEFAULT_CODEX_MODEL_ID.to_string(),
            effort: DEFAULT_EFFORT.to_string(),
            agent_access: AgentAccess::FullAccess,
        }
    }
}

/// A fully resolved configuration, with no inherited fields left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfiguration {
    pub provider_id: String,
    pub model_id: String,
    pub effort: String,
    pub agent_mode: AgentMode,
    pub agent_access: AgentAccess,
    pub system_prompt: Option<String>,
    pub context_window_size: Option<u64>,
}

/// The provider used when a thread states none.
pub const DEFAULT_PROVIDER_ID: &str = "codex";
/// The model used when a thread states none.
pub const DEFAULT_CODEX_MODEL_ID: &str = "gpt-5.6-sol";
/// The reasoning effort used when a thread states none.
pub const DEFAULT_EFFORT: &str = "medium";

/// Every reasoning effort any supported provider advertises.
pub const ALL_EFFORTS: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_configuration_inherits_everything() {
        let c = AgentConfiguration::default();
        assert!(!c.has_mode_or_access_override());
        assert_eq!(c.resolved_mode(), AgentMode::Auto);
        assert_eq!(
            c.resolved_access(AgentAccess::Supervised),
            AgentAccess::Supervised,
            "an unset access must take the app default, not its own"
        );
    }

    #[test]
    fn an_explicit_access_beats_the_app_default() {
        let c = AgentConfiguration {
            agent_access: Some(AgentAccess::Supervised),
            ..Default::default()
        };
        assert_eq!(
            c.resolved_access(AgentAccess::FullAccess),
            AgentAccess::Supervised
        );
    }

    #[test]
    fn an_inheriting_configuration_round_trips_without_gaining_an_override() {
        // The subtle case: a provider-native thread has both mode and access
        // unset. If persisting inferred a value, the next load would turn
        // inheritance into a hard override.
        let c = AgentConfiguration {
            model_id: Some("gpt-5.6-sol".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            !json.contains("agentMode") && !json.contains("agentAccess"),
            "an inheriting config persisted an override: {json}"
        );

        let back: AgentConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_mode, None);
        assert_eq!(back.agent_access, None);
        assert!(!back.has_mode_or_access_override());
    }

    #[test]
    fn a_configured_thread_round_trips_its_override() {
        let c = AgentConfiguration {
            agent_mode: Some(AgentMode::Plan),
            agent_access: Some(AgentAccess::AcceptEdits),
            context_window_size: Some(272_000),
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: AgentConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn a_partial_override_stays_partial() {
        // Only mode is pinned. Access must still inherit.
        let c = AgentConfiguration {
            agent_mode: Some(AgentMode::Plan),
            ..Default::default()
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("agentAccess"), "{json}");

        let back: AgentConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_mode, Some(AgentMode::Plan));
        assert_eq!(back.agent_access, None);
        assert_eq!(
            back.resolved_access(AgentAccess::Supervised),
            AgentAccess::Supervised
        );
    }

    #[test]
    fn an_unknown_field_in_a_payload_is_ignored() {
        // A payload written by a later build must still load.
        let json = r#"{"model_id":"gpt-5.6-sol","something_new":true}"#;
        let c: AgentConfiguration = serde_json::from_str(json).unwrap();
        assert_eq!(c.model_id.as_deref(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn an_explicit_override_outranks_the_providers_recorded_value() {
        let native = NativeDefaults {
            model_id: Some("native-model".into()),
            agent_access: Some(AgentAccess::Supervised),
            ..Default::default()
        };
        let c = AgentConfiguration {
            model_id: Some("chosen".into()),
            ..Default::default()
        };
        let r = c.resolve(&native, &AppDefaults::default());
        assert_eq!(r.model_id, "chosen", "an explicit model must win");
        assert_eq!(
            r.agent_access,
            AgentAccess::Supervised,
            "an unset access still inherits the provider's value"
        );
    }

    #[test]
    fn resolution_walks_override_then_native_then_app_default() {
        let native = NativeDefaults {
            provider_id: Some("claude".into()),
            model_id: Some("claude-fable-5".into()),
            effort: Some("high".into()),
            agent_mode: Some(AgentMode::Plan),
            agent_access: Some(AgentAccess::AcceptEdits),
            ..Default::default()
        };
        let r = AgentConfiguration::default().resolve(&native, &AppDefaults::default());
        assert_eq!(r.provider_id, "claude");
        assert_eq!(r.model_id, "claude-fable-5");
        assert_eq!(r.effort, "high");
        assert_eq!(r.agent_mode, AgentMode::Plan);
        assert_eq!(r.agent_access, AgentAccess::AcceptEdits);
    }

    #[test]
    fn a_bare_thread_falls_all_the_way_through_to_app_defaults() {
        let r = AgentConfiguration::default()
            .resolve(&NativeDefaults::default(), &AppDefaults::default());
        assert_eq!(r.provider_id, DEFAULT_PROVIDER_ID);
        assert_eq!(r.model_id, DEFAULT_CODEX_MODEL_ID);
        assert_eq!(r.effort, DEFAULT_EFFORT);
        assert_eq!(r.agent_mode, AgentMode::Auto);
        assert_eq!(r.agent_access, AgentAccess::FullAccess);
    }

    #[test]
    fn a_context_window_only_carries_through_when_both_other_sources_are_silent() {
        let native = NativeDefaults {
            context_window_size: Some(400_000),
            ..Default::default()
        };
        let r = AgentConfiguration::default().resolve(&native, &AppDefaults::default());
        assert_eq!(r.context_window_size, Some(400_000));

        let pinned = AgentConfiguration {
            context_window_size: Some(200_000),
            ..Default::default()
        };
        assert_eq!(
            pinned
                .resolve(&native, &AppDefaults::default())
                .context_window_size,
            Some(200_000)
        );

        let neither = AgentConfiguration::default()
            .resolve(&NativeDefaults::default(), &AppDefaults::default());
        assert_eq!(neither.context_window_size, None);
    }

    #[test]
    fn the_effort_list_covers_every_provider_advertised_value() {
        for expected in ["low", "medium", "high", "xhigh", "max", "ultra", "none"] {
            assert!(ALL_EFFORTS.contains(&expected), "missing effort {expected}");
        }
    }

    #[test]
    fn access_and_mode_labels_and_explanations_are_present() {
        for a in AgentAccess::ALL {
            assert!(!a.label().is_empty());
            assert!(!a.explanation().is_empty());
        }
        for m in AgentMode::ALL {
            assert!(!m.label().is_empty());
            assert!(!m.explanation().is_empty());
        }
    }
}
