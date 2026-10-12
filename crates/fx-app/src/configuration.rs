//! Thread-scoped configuration menus. An existing provider session keeps its provider.

use fx_core::{Agent, AgentAccess, AgentMode, DEFAULT_CODEX_MODEL_ID};
use fx_design::components::{MenuItem, MenuModel, MenuSection, MenuTrigger, menu_trigger};
use fx_design::styled::px_len;
use fx_design::theme::Spacing;
use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, SharedString, div};

use crate::AppState;

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
        models.push((current.clone(), current.clone()));
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
    crate::provider_catalog::models(provider(agent))
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| m.supported_efforts)
        .unwrap_or_else(|| {
            ["low", "medium", "high"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
}

fn provider_is_locked(state: &mut AppState, id: uuid::Uuid) -> bool {
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
    let conversation = state.conversation(project_id, id);
    let conversation = conversation.lock().unwrap_or_else(|e| e.into_inner());
    conversation.session_id().is_some() || conversation.messages().len() > 0
}

pub fn bar(state: &Entity<AppState>, cx: &mut App) -> AnyElement {
    let Some(agent) = state.read(cx).active_agent() else {
        return div().into_any_element();
    };
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
            "mode",
            agent.configuration.resolved_mode().label(),
            true,
            AgentMode::ALL
                .iter()
                .map(|mode| {
                    MenuItem::new(mode.label(), mode.label())
                        .subtitle(mode.explanation())
                        .selected(*mode == agent.configuration.resolved_mode())
                })
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
        let trigger = MenuTrigger::new(
            MenuModel::new(vec![MenuSection::new(field, items)]),
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
        let Some(agent) = state
            .snapshot
            .projects
            .iter_mut()
            .flat_map(|p| &mut p.agents)
            .find(|a| a.id == id)
        else {
            return;
        };
        match field {
            "provider" if ["codex", "claude"].contains(&chosen) => {
                if provider(agent) == chosen {
                    return;
                }
                agent.configuration.provider_id = Some(chosen.into());
                agent.configuration.model_id = None;
                agent.configuration.context_window_size = None;
                agent.configuration.effort = None;
            }
            "model" if models(agent).iter().any(|(id, _)| id == chosen) => {
                agent.configuration.model_id = (chosen != "default").then(|| chosen.into());
                agent.configuration.context_window_size =
                    crate::provider_catalog::models(provider(agent))
                        .into_iter()
                        .find(|model| model.id == chosen)
                        .map(|model| model.context_window)
                        .filter(|size| *size > 0);
                if agent
                    .configuration
                    .effort
                    .as_ref()
                    .is_some_and(|e| !efforts(agent).contains(e))
                {
                    agent.configuration.effort = None;
                }
            }
            "effort" if efforts(agent).iter().any(|id| id == chosen) => {
                agent.configuration.effort = Some(chosen.into())
            }
            "mode" => {
                let Some(mode) = AgentMode::ALL
                    .into_iter()
                    .find(|mode| mode.label() == chosen)
                else {
                    return;
                };
                agent.configuration.agent_mode = Some(mode);
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
                &format!("configuration:{}:mode", second.id),
                "Plan",
                cx,
            );
            apply(
                &state,
                &format!("configuration:{}:access", second.id),
                "Supervised",
                cx,
            );
            apply(
                &state,
                &format!("configuration:{}:effort", second.id),
                "high",
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
        assert_eq!(config.agent_mode, Some(AgentMode::Plan));
        assert_eq!(config.agent_access, Some(AgentAccess::Supervised));
        assert_eq!(config.effort.as_deref(), Some("high"));
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
}
