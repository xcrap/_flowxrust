//! Thread-scoped configuration menus. An existing provider session keeps its provider.

use fx_core::{Agent, AgentAccess, DEFAULT_CODEX_MODEL_ID, NativeDefaults};
use fx_design::components::{MenuItem, MenuModel, MenuSection, MenuTrigger, menu_trigger};
use fx_design::styled::px_len;
use fx_design::theme::Spacing;
use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, SharedString, div};

use crate::AppState;

/// Resolve inherited provider settings for display or a turn without turning
/// them into persisted user overrides.
pub(crate) fn with_native_defaults(mut agent: Agent, native: NativeDefaults) -> Agent {
    let imported = native.provider_id.is_some();
    let configuration = &mut agent.configuration;
    configuration.provider_id = configuration.provider_id.take().or(native.provider_id);
    configuration.model_id = configuration.model_id.take().or(native.model_id);
    configuration.effort = configuration.effort.take().or(native.effort);
    configuration.agent_access = configuration
        .agent_access
        .or(native.agent_access)
        .or(imported.then_some(AgentAccess::Supervised));
    configuration.context_window_size = configuration
        .context_window_size
        .or(native.context_window_size);
    agent
}

fn provider(agent: &Agent) -> &str {
    agent
        .configuration
        .provider_id
        .as_deref()
        .unwrap_or("codex")
}

fn models(agent: &Agent) -> Vec<(String, String)> {
    let mut models: Vec<_> = crate::provider_catalog::models(provider(agent))
        .into_iter()
        .map(|model| (model.id, model.display_name))
        .collect();
    if let Some(current) = &agent.configuration.model_id
        && !models.iter().any(|(id, _)| id == current)
    {
        if let Some(model) = crate::provider_catalog::model(provider(agent), current) {
            if let Some(choice) = models.iter_mut().find(|(id, _)| id == &model.id) {
                choice.0 = current.clone();
            } else {
                models.push((current.clone(), model.display_name));
            }
        } else {
            models.push((current.clone(), current.clone()));
        }
    }
    models
}

fn efforts(agent: &Agent) -> Vec<String> {
    let id = agent
        .configuration
        .model_id
        .as_deref()
        .unwrap_or(if provider(agent) == "claude" {
            "default"
        } else {
            DEFAULT_CODEX_MODEL_ID
        });
    crate::provider_catalog::model(provider(agent), id)
        .map(|m| m.supported_efforts)
        .unwrap_or_default()
}

fn provider_is_locked(state: &mut AppState, id: uuid::Uuid) -> bool {
    if state.native_defaults(id).provider_id.is_some() {
        return true;
    }
    let Some(project_id) = state.snapshot.projects.iter().find_map(|record| {
        record
            .agents
            .iter()
            .any(|agent| agent.id == id)
            .then(|| record.project.as_ref().map(|p| p.id))
            .flatten()
    }) else {
        return true;
    };
    // Drawn with the composer, so it must not read a transcript. One that is
    // saved but not loaded yet almost always has history; locking until it
    // lands is the safe answer, and the bar repaints when it does.
    let Some(conversation) = state.loaded_or_unsaved(project_id, id) else {
        return true;
    };
    let conversation = conversation.lock().unwrap_or_else(|e| e.into_inner());
    conversation.session_id().is_some() || conversation.messages().len() > 0
}

pub fn bar(state: &Entity<AppState>, cx: &mut App) -> AnyElement {
    let Some(agent) = state.read(cx).active_agent() else {
        return div().into_any_element();
    };
    let agent = with_native_defaults(agent.clone(), state.read(cx).native_defaults(agent.id));
    let locked = state.update(cx, |state, _| provider_is_locked(state, agent.id));
    let busy = state.read(cx).runners.contains_key(&agent.id);
    let layer = state.read(cx).menu_layer.clone();
    let selected_model =
        agent
            .configuration
            .model_id
            .as_deref()
            .unwrap_or(if provider(&agent) == "claude" {
                "default"
            } else {
                DEFAULT_CODEX_MODEL_ID
            });
    let effective_effort = crate::provider_catalog::effective_effort(
        provider(&agent),
        selected_model,
        agent.configuration.effort.as_deref(),
    );
    let selected_effort = effective_effort.as_deref().unwrap_or("Provider default");
    let model_choices = models(&agent);
    let model_label = model_choices
        .iter()
        .find(|(id, _)| id == selected_model)
        .map(|(_, label)| label.as_str())
        .unwrap_or(selected_model);
    let fields = vec![
        (
            "provider",
            if provider(&agent) == "claude" {
                "Claude"
            } else {
                "Codex"
            },
            !locked,
            vec![
                MenuItem::new("codex", "Codex").selected(provider(&agent) == "codex"),
                MenuItem::new("claude", "Claude").selected(provider(&agent) == "claude"),
            ],
        ),
        (
            "model",
            model_label,
            true,
            model_choices
                .iter()
                .map(|(id, label)| MenuItem::new(id, label).selected(id == selected_model))
                .collect(),
        ),
        (
            "effort",
            selected_effort,
            true,
            efforts(&agent)
                .iter()
                .map(|e| MenuItem::new(e, e).selected(e == selected_effort))
                .collect(),
        ),
        (
            "access",
            agent
                .configuration
                .resolved_access(AgentAccess::default())
                .label(),
            true,
            AgentAccess::ALL
                .iter()
                .map(|access| {
                    MenuItem::new(access.label(), access.label())
                        .subtitle(access.explanation())
                        .selected(
                            *access == agent.configuration.resolved_access(AgentAccess::default()),
                        )
                })
                .collect(),
        ),
    ];
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px_len(Spacing::XXS));
    for (field, label, enabled, items) in fields {
        if items.is_empty() {
            continue;
        }
        let owner = format!("configuration:{}:{field}", agent.id);
        let mut sections = vec![MenuSection::new(field, items)];
        if field == "model" {
            sections.push(MenuSection::new(
                "catalog",
                vec![MenuItem::new("__refresh-models", "Refresh models")],
            ));
        }
        let trigger = MenuTrigger::new(
            MenuModel::new(sections),
            SharedString::from(owner.clone()),
            SharedString::from(label.to_string()),
        )
        .enabled(!busy && enabled)
        .expanded(layer.read(cx).is_owner(&owner));
        row = row.child(menu_trigger(trigger, &layer));
    }
    use fx_design::styled::FxStyled;
    use fx_design::theme::{Colors, global};
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(row)
        .children(
            crate::provider_catalog::warning(provider(&agent)).map(|detail| {
                div()
                    .id("model-catalog-warning")
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&global::current()))
                    .child("Using saved model choices")
                    .tooltip(move |window, cx| {
                        fx_design::components::tooltip(detail.clone(), window, cx)
                    })
            }),
        )
        .into_any_element()
}

pub fn apply(state: &Entity<AppState>, owner: &str, chosen: &str, cx: &mut App) {
    let mut parts = owner.split(':');
    if parts.next() != Some("configuration") {
        return;
    }
    let Some(id) = parts.next().and_then(|id| uuid::Uuid::parse_str(id).ok()) else {
        return;
    };
    let Some(field) = parts.next() else {
        return;
    };
    if parts.next().is_some() {
        return;
    }
    state.update(cx, |state, cx| {
        if state.runners.contains_key(&id) {
            return;
        }
        if field == "provider" && provider_is_locked(state, id) {
            return;
        }
        let provider_defaults = (field == "provider" && ["codex", "claude"].contains(&chosen))
            .then(|| state.default_configuration(chosen));
        let native = state.native_defaults(id);
        let Some(agent) = state
            .snapshot
            .projects
            .iter_mut()
            .flat_map(|p| &mut p.agents)
            .find(|a| a.id == id)
        else {
            return;
        };
        let effective = with_native_defaults(agent.clone(), native.clone());
        match field {
            "model" if chosen == "__refresh-models" => {
                crate::provider_catalog::refresh_now(provider(&effective));
                cx.notify();
                return;
            }
            "provider" if ["codex", "claude"].contains(&chosen) => {
                if provider(agent) == chosen {
                    return;
                }
                let system_prompt = agent.configuration.system_prompt.take();
                agent.configuration = provider_defaults.expect("validated provider defaults");
                agent.configuration.system_prompt = system_prompt;
            }
            "model" if models(&effective).iter().any(|(id, _)| id == chosen) => {
                agent.configuration.model_id = Some(chosen.into());
                agent.configuration.context_window_size =
                    crate::provider_catalog::model(provider(&effective), chosen)
                        .map(|model| model.context_window)
                        .filter(|size| *size > 0);
                if agent.configuration.effort.as_ref().is_some_and(|e| {
                    !efforts(&with_native_defaults(agent.clone(), native)).contains(e)
                }) {
                    agent.configuration.effort = None;
                }
            }
            "effort" if efforts(&effective).iter().any(|id| id == chosen) => {
                agent.configuration.effort = Some(chosen.into())
            }
            "access" => {
                let Some(access) = AgentAccess::ALL
                    .into_iter()
                    .find(|access| access.label() == chosen)
                else {
                    return;
                };
                agent.configuration.agent_access = Some(access);
            }
            _ => return,
        }
        state.save();
        cx.notify();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::{ConversationMessage, MessageRole, Project};
    use fx_store::{AppPaths, PersistedApp, PersistedProject, Store};

    #[test]
    fn imported_settings_are_inherited_without_becoming_user_overrides() {
        let agent = Agent::new("Imported");
        let native = NativeDefaults {
            provider_id: Some("claude".into()),
            model_id: Some("native-model".into()),
            effort: Some("high".into()),
            agent_access: Some(AgentAccess::AcceptEdits),
            context_window_size: Some(200_000),
        };
        let effective = with_native_defaults(agent.clone(), native.clone());
        assert_eq!(provider(&effective), "claude");
        assert_eq!(effective.configuration.model_id, native.model_id);
        assert_eq!(effective.configuration.effort, native.effort);
        assert_eq!(
            effective.configuration.agent_access,
            Some(AgentAccess::AcceptEdits)
        );
        assert!(agent.configuration.model_id.is_none());
        assert!(agent.configuration.agent_access.is_none());

        let mut overridden = agent;
        overridden.configuration.model_id = Some("default".into());
        overridden.configuration.agent_access = Some(AgentAccess::Supervised);
        let effective = with_native_defaults(overridden, native);
        assert_eq!(effective.configuration.model_id.as_deref(), Some("default"));
        assert_eq!(
            effective.configuration.agent_access,
            Some(AgentAccess::Supervised)
        );
    }

    #[test]
    fn unknown_native_access_does_not_become_full_access() {
        let effective = with_native_defaults(
            Agent::new("Imported"),
            NativeDefaults {
                provider_id: Some("codex".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            effective.configuration.agent_access,
            Some(AgentAccess::Supervised)
        );
    }

    #[test]
    fn menu_choices_target_their_thread_and_survive_restart() {
        let test = gpui::TestAppContext::single();
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let first = Agent::new("First");
        let second = Agent::new("Second");
        let snapshot = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(Project::new(dir.path().to_str().unwrap())),
                agents: vec![first.clone(), second.clone()],
                ..Default::default()
            }],
            ..Default::default()
        };
        Store::new(paths.clone()).save_app(&snapshot).unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            apply(
                &state,
                &format!("configuration:{}:provider", second.id),
                "claude",
                cx,
            );
            apply(
                &state,
                &format!("configuration:{}:access", second.id),
                "Supervised",
                cx,
            );
            // A stale or forged selection cannot add a model we never offered.
            apply(
                &state,
                &format!("configuration:{}:model", second.id),
                "unknown-model",
                cx,
            );
        });
        let saved = Store::new(paths).load_app().unwrap();
        assert_eq!(
            saved.projects[0].agents[0].configuration,
            first.configuration
        );
        let config = &saved.projects[0].agents[1].configuration;
        assert_eq!(config.provider_id.as_deref(), Some("claude"));
        assert_eq!(config.agent_access, Some(AgentAccess::Supervised));
        assert!(config.effort.is_none());
        assert!(config.model_id.is_none());
    }

    #[test]
    fn provider_cannot_change_after_a_conversation_has_history() {
        let test = gpui::TestAppContext::single();
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let agent = Agent::new("Existing");
        Store::new(paths.clone())
            .save_app(&PersistedApp {
                projects: vec![PersistedProject {
                    project: Some(Project::new(dir.path().to_str().unwrap())),
                    agents: vec![agent.clone()],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            let conversation = state.update(cx, |state, _| {
                assert!(!provider_is_locked(state, agent.id));
                state.conversation_by_agent(agent.id).unwrap()
            });
            conversation
                .lock()
                .unwrap()
                .append(ConversationMessage::text(
                    MessageRole::User,
                    "Original prompt",
                ));
            apply(
                &state,
                &format!("configuration:{}:provider", agent.id),
                "claude",
                cx,
            );
            assert!(
                state.read(cx).snapshot.projects[0].agents[0]
                    .configuration
                    .provider_id
                    .is_none()
            );
        });
    }

    #[test]
    fn the_provider_lock_never_reads_a_saved_transcript() {
        // The lock is drawn with every composer, so reading the transcript here
        // meant reading all of it inside a render.
        let test = gpui::TestAppContext::single();
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let project = Project::new(dir.path().to_str().unwrap());
        let agent = Agent::new("Saved");
        let store = Store::new(paths.clone());
        store
            .save_app(&PersistedApp {
                projects: vec![PersistedProject {
                    project: Some(project.clone()),
                    agents: vec![agent.clone()],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        store
            .save_conversation(
                project.id,
                agent.id,
                &fx_store::PersistedConversation::from_messages(
                    agent.id,
                    vec![ConversationMessage::text(MessageRole::User, "History")],
                ),
            )
            .unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            state.update(cx, |state, _| {
                assert!(
                    provider_is_locked(state, agent.id),
                    "an unread transcript is taken to have history"
                );
                assert!(state.conversation_by_agent(agent.id).is_none());
                assert!(!state.is_loading_conversation(agent.id));
                state.conversation(project.id, agent.id);
                assert!(provider_is_locked(state, agent.id));
            })
        });
    }

    #[test]
    fn switching_an_unused_chat_applies_target_defaults_and_retains_system_prompt() {
        let test = gpui::TestAppContext::single();
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let mut agent = Agent::new("Unused");
        agent.configuration.system_prompt = Some("Project-specific instructions".into());
        agent.configuration.agent_access = Some(AgentAccess::FullAccess);
        agent.configuration.context_window_size = Some(10);
        Store::new(paths.clone())
            .save_app(&PersistedApp {
                projects: vec![PersistedProject {
                    project: Some(Project::new(dir.path().to_str().unwrap())),
                    agents: vec![agent.clone()],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.preferences.claude_defaults = fx_store::ProviderDefaults {
                    model_id: Some("custom-claude-model".into()),
                    effort: Some("high".into()),
                    agent_access: Some(AgentAccess::Supervised),
                };
                state.preferences.codex_defaults = fx_store::ProviderDefaults {
                    model_id: Some("gpt-6-astra".into()),
                    effort: Some("unsupported-effort".into()),
                    agent_access: Some(AgentAccess::AcceptEdits),
                };
            });
            let owner = format!("configuration:{}:provider", agent.id);
            apply(&state, &owner, "claude", cx);
            let saved = Store::new(paths.clone()).load_app().unwrap();
            let configuration = &saved.projects[0].agents[0].configuration;
            assert_eq!(configuration.provider_id.as_deref(), Some("claude"));
            assert_eq!(
                configuration.model_id.as_deref(),
                Some("custom-claude-model")
            );
            assert_eq!(configuration.effort.as_deref(), Some("high"));
            assert_eq!(configuration.agent_access, Some(AgentAccess::Supervised));
            assert!(configuration.context_window_size.is_none());
            assert_eq!(
                configuration.system_prompt,
                agent.configuration.system_prompt
            );
            apply(&state, &owner, "codex", cx);
            let saved = Store::new(paths).load_app().unwrap();
            let configuration = &saved.projects[0].agents[0].configuration;
            assert_eq!(configuration.provider_id.as_deref(), Some("codex"));
            assert_eq!(configuration.model_id.as_deref(), Some("gpt-6-astra"));
            assert!(configuration.effort.is_none());
            assert_eq!(configuration.agent_access, Some(AgentAccess::AcceptEdits));
            assert_eq!(configuration.context_window_size, Some(272_000));
            assert_eq!(
                configuration.system_prompt,
                agent.configuration.system_prompt
            );
        });
    }
}
