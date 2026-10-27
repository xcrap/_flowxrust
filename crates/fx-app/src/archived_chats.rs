//! Recovery for archived chats, reachable only from Settings.

use fx_design::components::{ButtonStyle, button, settings_group, settings_row_custom};
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, Spacing, global};
use gpui::prelude::*;
use gpui::{App, Context, Entity, SharedString, Subscription, Window, div};
use uuid::Uuid;

use crate::state::AppState;
use crate::text_field::TextField;

pub(crate) struct ArchivedChats {
    state: Entity<AppState>,
    search: Entity<TextField>,
    error: Option<String>,
    _observations: Vec<Subscription>,
}

impl ArchivedChats {
    pub fn new(state: Entity<AppState>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let search = cx.new(|cx| TextField::new(cx).placeholder("Search chats or projects"));
            let observations = vec![
                cx.observe(&search, |_, _, cx| cx.notify()),
                cx.observe(&state, |_, _, cx| cx.notify()),
            ];
            Self {
                state,
                search,
                error: None,
                _observations: observations,
            }
        })
    }

    pub fn focus_search(&self, window: &mut Window, cx: &App) {
        self.search.read(cx).focus_handle().focus(window);
    }

    fn restore(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let success = self.state.update(cx, |state, cx| {
            let restored = state.set_thread_archived(id, false);
            if restored {
                // Shown the way ⌘1 or the palette shows a thread: in its
                // project, opened if it was collapsed, with that project's
                // provider threads asked for. `select_agent` alone left the
                // restored chat inside a collapsed project.
                state.open_thread(id, cx);
                state.save();
            }
            cx.notify();
            restored
        });
        self.error =
            if success {
                None
            } else {
                Some(self.state.read(cx).last_error.clone().unwrap_or_else(|| {
                    "This chat cannot be restored while a turn is running.".into()
                }))
            };
        cx.notify();
    }
}

impl Render for ArchivedChats {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let state = self.state.read(cx);
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut count = 0;
        let mut visible = 0;
        let mut rows = Vec::new();
        for record in &state.snapshot.projects {
            let Some(project) = &record.project else {
                continue;
            };
            for agent in &record.agents {
                if !record.archived_agent_ids.contains(&agent.id) {
                    continue;
                }
                count += 1;
                let title = if agent.title.trim().is_empty() {
                    "Untitled chat"
                } else {
                    &agent.title
                };
                if !query.is_empty()
                    && !title.to_lowercase().contains(&query)
                    && !project.name.to_lowercase().contains(&query)
                {
                    continue;
                }
                let id = agent.id;
                visible += 1;
                let provider = match agent.configuration.provider_id.as_deref().or_else(|| {
                    record
                        .native_bindings
                        .iter()
                        .find(|binding| binding.agent_id == id)
                        .map(|binding| binding.provider_id.as_str())
                }) {
                    Some("claude") => "Claude",
                    _ => "Codex",
                };
                let this = cx.entity();
                let full_title = title.to_owned();
                rows.push(
                    div()
                        .id(SharedString::from(format!("archived-chat:{id}")))
                        .debug_selector(move || format!("archived-chat:{id}"))
                        .w_full()
                        .child(settings_row_custom(
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px_len(Spacing::XXXS))
                                .child(
                                    div()
                                        .id("archive-title")
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .fx_body_medium()
                                        .text_color(Colors::fg(&theme))
                                        .child(SharedString::from(title.to_owned()))
                                        .tooltip(move |window, cx| {
                                            fx_design::components::tooltip(
                                                full_title.clone(),
                                                window,
                                                cx,
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .fx_caption()
                                        .text_color(Colors::fg_secondary(&theme))
                                        .child(SharedString::from(format!(
                                            "{} · {provider}",
                                            project.name
                                        ))),
                                ),
                            div()
                                .debug_selector(move || format!("restore-archived:{id}"))
                                .child(button(
                                    "Restore",
                                    ButtonStyle::Secondary,
                                    !state.is_thread_busy(id),
                                    move |_, _, cx| {
                                        this.update(cx, |this, cx| this.restore(id, cx))
                                    },
                                )),
                        ))
                        .into_any_element(),
                );
            }
        }
        let mut body = div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::MD))
            .child(self.search.clone());
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .fx_caption()
                    .text_color(Colors::error(&theme))
                    .child(SharedString::from(error.clone())),
            );
        }
        if visible == 0 {
            body = body.child(settings_group([settings_row_custom(
                div()
                    .py(px_len(Spacing::SM))
                    .fx_body()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(if count == 0 {
                        "No archived chats"
                    } else {
                        "No matching archived chats"
                    }),
                div(),
            )
            .into_any_element()]));
        } else {
            body = body.child(settings_group(rows));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::{Agent, ConversationMessage, MessageRole, Project};
    use fx_store::{AppPaths, PersistedApp, PersistedConversation, PersistedProject, Store};
    use gpui::{MouseButton, TestAppContext, VisualTestContext, px, size};

    #[test]
    fn settings_search_and_restore_preserve_history_and_return_the_chat_to_its_project() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(directory.path());
        let store = Store::new(paths.clone());
        let mut project = Project::new(directory.path().to_string_lossy());
        project.name = "Recovery project".into();
        let first = Agent::new("Unique archived conversation");
        let second = Agent::new("Another conversation");
        // GPUI's test selector API retains static strings.
        let first_selector: &'static str =
            Box::leak(format!("archived-chat:{}", first.id).into_boxed_str());
        let second_selector: &'static str =
            Box::leak(format!("archived-chat:{}", second.id).into_boxed_str());
        let restore_selector: &'static str =
            Box::leak(format!("restore-archived:{}", first.id).into_boxed_str());
        project.agent_order = vec![first.id, second.id];
        store
            .save_app(&PersistedApp {
                projects: vec![PersistedProject {
                    project: Some(project.clone()),
                    agents: vec![first.clone(), second.clone()],
                    archived_agent_ids: vec![first.id, second.id],
                    missing_native_agent_ids: vec![first.id],
                    is_expanded: true,
                    ..Default::default()
                }],
                active_project_id: Some(project.id),
                ..Default::default()
            })
            .unwrap();
        let mut history = PersistedConversation::from_messages(
            first.id,
            vec![ConversationMessage::text(
                MessageRole::User,
                "Saved history",
            )],
        );
        history.draft_text = "Unsent draft".into();
        store
            .save_conversation(project.id, first.id, &history)
            .unwrap();
        let mut test = TestAppContext::single();
        let (state, archives) = test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths.clone(), cx));
            let archives = ArchivedChats::new(state.clone(), cx);
            (state, archives)
        });
        let host_state = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(host_state));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1100.0), px(900.0)));
        window
            .update(&mut test, |_, window, cx| {
                let settings = crate::settings::SettingsSheet::open(
                    window,
                    cx,
                    Box::new(|_, _, _| {}),
                    Box::new(|_, _| {}),
                    Box::new(|_, _| {}),
                );
                settings.update(cx, |settings, _| settings.set_archives(archives.clone()));
                crate::workspace_ui::handle(cx).update(cx, |ui, cx| {
                    ui.settings = Some(settings);
                    cx.notify();
                });
                cx.notify();
            })
            .unwrap();
        test.refresh().unwrap();
        let position = visual
            .debug_bounds("manage-archived-chats")
            .unwrap()
            .center();
        visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
        test.refresh().unwrap();
        assert!(visual.debug_bounds(first_selector).is_some());
        assert!(visual.debug_bounds(second_selector).is_some());
        test.simulate_input(window.into(), "UNIQUE");
        test.read(|cx| {
            assert_eq!(
                archives.read(cx).search.read(cx).value(),
                "UNIQUE",
                "search should own keyboard input"
            )
        });
        test.refresh().unwrap();
        assert!(visual.debug_bounds(first_selector).is_some());
        let position = visual.debug_bounds(restore_selector).unwrap().center();
        visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
        test.refresh().unwrap();
        test.read(|cx| {
            let state = state.read(cx);
            assert!(!state.is_thread_archived(first.id));
            assert!(state.is_thread_archived(second.id));
            assert_eq!(state.snapshot.active_agent_id, Some(first.id));
            assert!(
                !state.snapshot.projects[0]
                    .missing_native_agent_ids
                    .contains(&first.id)
            );
        });
        assert_eq!(
            store.load_app().unwrap().projects[0].archived_agent_ids,
            vec![second.id]
        );
        let restored = store
            .load_conversation(project.id, first.id)
            .unwrap()
            .unwrap();
        assert_eq!(restored.messages, history.messages);
        assert_eq!(restored.draft_text, "Unsent draft");
    }

    #[test]
    fn a_chat_restored_into_a_collapsed_project_is_shown_open_in_it() {
        // Restoring selected the chat and nothing more, so it came back inside
        // a project the sidebar still had folded away.
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(directory.path());
        let open = Agent::new("Open elsewhere");
        let archived = Agent::new("Archived");
        let root = |name: &str| {
            let root = directory.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            root.to_string_lossy().into_owned()
        };
        let mut current = Project::new(root("current"));
        current.agent_order = vec![open.id];
        let mut folded = Project::new(root("folded"));
        folded.agent_order = vec![archived.id];
        let folded_id = folded.id;
        Store::new(paths.clone())
            .save_app(&PersistedApp {
                active_project_id: Some(current.id),
                active_agent_id: Some(open.id),
                projects: vec![
                    PersistedProject {
                        project: Some(current),
                        agents: vec![open],
                        is_expanded: true,
                        ..Default::default()
                    },
                    PersistedProject {
                        project: Some(folded),
                        agents: vec![archived.clone()],
                        archived_agent_ids: vec![archived.id],
                        is_expanded: false,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            })
            .unwrap();
        let test = TestAppContext::single();
        let (state, archives) = test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths.clone(), cx));
            let archives = ArchivedChats::new(state.clone(), cx);
            (state, archives)
        });
        test.update(|cx| archives.update(cx, |archives, cx| archives.restore(archived.id, cx)));
        test.read(|cx| {
            let state = state.read(cx);
            assert!(!state.is_thread_archived(archived.id));
            assert_eq!(state.snapshot.active_project_id, Some(folded_id));
            assert_eq!(state.snapshot.active_agent_id, Some(archived.id));
            assert!(
                state.is_expanded(folded_id),
                "restored into a folded project"
            );
        });
        let saved = Store::new(paths).load_app().unwrap();
        assert!(saved.projects[1].is_expanded);
    }
}
