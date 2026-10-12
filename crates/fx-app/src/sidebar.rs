//! The sidebar: projects and their threads.

use fx_core::{Agent, Project};
use fx_design::components::{
    ActivityState, ButtonStyle, DividerAxis, IconButtonProps, MenuItem, MenuItemTone, MenuModel,
    MenuSection, activity_dot, button_with_icon, context_region, divider_with_axis, empty_state,
    icon_button,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{AnyElement, App, Entity, Rgba, SharedString, Window, div, px};
use uuid::Uuid;

use crate::state::AppState;

/// Height of a project row.
const PROJECT_ROW_HEIGHT: f32 = Layout::SIDEBAR_ROW_HEIGHT;
/// Height of a thread row.
const THREAD_ROW_HEIGHT: f32 = Layout::SIDEBAR_ROW_HEIGHT;
/// Height of the sidebar's own header.
const SIDEBAR_HEADER_HEIGHT: f32 = Layout::PANE_HEADER_HEIGHT;

/// A fill that paints nothing, so a row can have a background or not.
fn no_fill() -> Rgba {
    Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    }
}

/// Render the sidebar column, or collapse it to nothing when hidden.
///
/// Collapsing rather than unmounting keeps the layout arithmetic simple: the
/// centre column's available width does not depend on whether a branch is
/// present in the tree.
pub fn view(state: Entity<AppState>, _window: &mut Window, cx: &mut App) -> AnyElement {
    let visible = state.read(cx).sidebar_visible;
    if !visible {
        return div().w(px(0.0)).h_full().flex_shrink_0().into_any_element();
    }

    let theme = global::current();
    div()
        .w(px(Layout::SIDEBAR_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(Colors::bg_elevated(&theme))
        .border_r(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .child(header(state.clone(), cx))
        .child(divider_with_axis(DividerAxis::Horizontal))
        .child(body(state, cx))
        .into_any_element()
}

fn header(state: Entity<AppState>, _cx: &mut App) -> AnyElement {
    let theme = global::current();
    let for_state = state.clone();
    div()
        .h(px(SIDEBAR_HEADER_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px(px_len(Spacing::MD))
        .gap(px_len(Spacing::XXS))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from("Projects")),
        )
        .child(icon_button(
            IconButtonProps::new(Icon::FOLDER_BADGE_PLUS, "Add project")
                .small()
                .ghost(),
            move |_event, window, cx| {
                crate::state::open_project_picker(for_state.clone(), window, cx);
            },
        ))
        .child(icon_button(
            IconButtonProps::new(Icon::GEARSHAPE, "Workspace actions")
                .small()
                .ghost(),
            move |event, _window, cx| {
                let model = crate::app_menu();
                let layer = state.read(cx).menu_layer.clone();
                layer.update(cx, |layer, cx| {
                    layer.open_at(model, "sidebar", event.position());
                    cx.notify();
                });
            },
        ))
        .into_any_element()
}

fn body(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let theme = global::current();
    let snapshot = state.read(cx).snapshot.clone();

    if snapshot.projects.is_empty() {
        return div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .p(px_len(Spacing::LG))
            .child(empty_state(
                Icon::FOLDER.render(
                    fx_design::theme::IconSize::Large,
                    Colors::fg_tertiary(&theme),
                    &theme,
                ),
                "No projects yet",
                "Add a folder to keep its threads together.",
            ))
            .child(button_with_icon(
                "Add project",
                Some(Icon::PLUS),
                ButtonStyle::Secondary,
                true,
                move |_, window, cx| crate::state::open_project_picker(state.clone(), window, cx),
            ))
            .into_any_element();
    }

    let mut list = div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        // See the inspector: scrolling is an interaction in GPUI 0.2.2, so the
        // element needs an id before it can scroll.
        .id("sidebar.projects")
        .overflow_y_scroll()
        .p(px_len(Spacing::SM))
        .gap(px_len(Spacing::SM));
    for record in &snapshot.projects {
        let Some(project) = record.project.as_ref() else {
            continue;
        };
        let id = project.id;
        let selected = Some(id) == snapshot.active_project_id;
        let mut group = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px_len(Spacing::XXXS))
            .child(project_row(
                state.clone(),
                id,
                project.clone(),
                selected,
                record.is_expanded,
                cx,
            ));
        if record.is_expanded {
            let native_status = state.read(cx).native_status(id);
            let loading = native_status.syncing && record.agents.is_empty();
            if loading || !native_status.warnings.is_empty() {
                let detail = native_status.warnings.join("\n");
                let label = if detail.is_empty() {
                    "Loading provider threads…"
                } else {
                    "Thread sync needs attention"
                };
                let retry_state = state.clone();
                group = group.child(
                    div()
                        .id(SharedString::from(format!("native-sync-status:{id}")))
                        .flex()
                        .items_center()
                        .gap(px_len(Spacing::XXS))
                        .pl(px_len(Spacing::XXL))
                        .pr(px_len(Spacing::XXS))
                        .py(px_len(Spacing::XXS))
                        .fx_caption()
                        .text_color(Colors::fg_secondary(&theme))
                        .child(div().flex_1().min_w_0().child(label))
                        .when(!detail.is_empty(), |row| {
                            row.tooltip(move |window, cx| {
                                fx_design::components::tooltip(detail.clone(), window, cx)
                            })
                            .child(icon_button(
                                IconButtonProps::new(Icon::ARROW_CLOCKWISE, "Retry thread sync")
                                    .tiny()
                                    .ghost()
                                    .enabled(!native_status.syncing),
                                move |_, _, cx| {
                                    retry_state.update(cx, |state, cx| {
                                        state.request_native_refresh(id);
                                        cx.notify();
                                    });
                                },
                            ))
                        }),
                );
            }
            let agents = project
                .agent_order
                .iter()
                .filter(|id| {
                    !record.archived_agent_ids.contains(id)
                        && !record.missing_native_agent_ids.contains(id)
                })
                .filter_map(|id| record.agents.iter().find(|agent| agent.id == *id))
                .cloned()
                .collect();
            if !loading {
                group = group.child(threads(&state, id, agents, snapshot.active_agent_id, cx));
            }
        }
        list = list.child(group);
    }
    list.into_any_element()
}

/// One project: a disclosure triangle, its name, and a per-row menu.
fn project_row(
    state: Entity<AppState>,
    id: Uuid,
    project: Project,
    selected: bool,
    expanded: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    let label = if project.name.trim().is_empty() {
        "Untitled project".to_string()
    } else {
        project.name.clone()
    };
    // Built before the closures below, so neither has to borrow the project.
    let busy = project
        .agent_order
        .iter()
        .any(|id| state.read(cx).is_thread_busy(*id));
    let menu = project_menu(&project.name, busy);
    let context_menu = menu.clone();
    let registry = state.read(cx).context_registry.clone();

    let row = div()
        .id(SharedString::from(format!("project:{id}")))
        .h(px(PROJECT_ROW_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::XXS))
        .pl(px_len(Spacing::XS))
        // The list's 8pt inset plus this 4pt inset matches the header's 12pt gutter.
        .pr(px_len(Spacing::XXS))
        .cursor_pointer()
        .rounded(px_len(Radii::SM))
        .hover(|s| s.fx_background(Colors::bg_hover(&theme)))
        .on_click({
            let state = state.clone();
            move |_, _window, cx| {
                state.update(cx, |state, cx| {
                    // Selecting the already-open project collapses it, which is
                    // the same gesture as the triangle and avoids a row that
                    // does nothing when clicked twice.
                    if state.snapshot.active_project_id == Some(id) {
                        state.toggle_project_expansion(id);
                    } else {
                        if !state.is_expanded(id) {
                            state.toggle_project_expansion(id);
                        }
                        state.select_project(id);
                    }
                    cx.notify();
                });
            }
        })
        .child(
            div()
                .w(px_len(Spacing::LG))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(
                    if expanded {
                        Icon::CHEVRON_DOWN
                    } else {
                        Icon::CHEVRON_RIGHT
                    }
                    .render(
                        IconSize::Micro,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ),
                ),
        )
        .child(
            div()
                .debug_selector(|| "sidebar-project-title".into())
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .fx_role(TextRole::CaptionMedium)
                .text_color(if selected {
                    Colors::fg(&theme)
                } else {
                    Colors::fg_secondary(&theme)
                })
                .child(SharedString::from(label)),
        )
        .child(icon_button(
            IconButtonProps::new(Icon::PLUS, "New thread")
                .tiny()
                .ghost(),
            {
                let state = state.clone();
                move |_, _, cx| {
                    cx.stop_propagation();
                    state.update(cx, |state, cx| {
                        state.save_all(cx);
                        state.apply_menu_choice(&format!("project:{id}"), "new-thread");
                        cx.notify();
                    });
                }
            },
        ))
        .child(icon_button(
            IconButtonProps::new(Icon::ELLIPSIS_CIRCLE, "Project actions")
                .tiny()
                .ghost(),
            move |event, _window, cx| {
                // The menu button is inside the selectable project row.
                cx.stop_propagation();
                let model = menu.clone();
                let layer = state.read(cx).menu_layer.clone();
                layer.update(cx, |layer, cx| {
                    let owner = format!("project:{id}");
                    if layer.is_owner(&owner) {
                        layer.dismiss();
                    } else {
                        layer.open_at(model, owner, event.position());
                    }
                    cx.notify();
                });
            },
        ));
    context_region(&registry, format!("project:{id}"), Some(context_menu), row).into_any_element()
}

/// A project's threads, indented under the project name.
fn threads(
    state: &Entity<AppState>,
    project_id: Uuid,
    agents: Vec<Agent>,
    active_agent: Option<Uuid>,
    cx: &mut App,
) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px_len(Spacing::XXXS))
        .pb(px_len(Spacing::XXS));
    if agents.is_empty() {
        column = column.child(
            div()
                .pl(px_len(Spacing::XXL))
                .py(px_len(Spacing::SM))
                .fx_caption()
                .text_color(Colors::fg_tertiary(&global::current()))
                .child("No threads yet"),
        );
    }
    for agent in agents {
        column = column.child(thread_row(
            state.clone(),
            project_id,
            agent.clone(),
            Some(agent.id) == active_agent,
            cx,
        ));
    }
    column.into_any_element()
}

fn thread_row(
    state: Entity<AppState>,
    project_id: Uuid,
    agent: Agent,
    selected: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    let agent_id = agent.id;
    let title = thread_title(&agent);
    let busy = state.read(cx).is_thread_busy(agent_id);
    let menu = thread_menu(false, busy);
    let context_menu = menu.clone();
    let registry = state.read(cx).context_registry.clone();

    let row = div()
        .id(SharedString::from(format!("thread:{agent_id}")))
        .debug_selector(move || format!("sidebar-thread:{agent_id}"))
        .h(px(THREAD_ROW_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM))
        // Past the project row's disclosure triangle and gutter.
        .pl(px_len(Spacing::SM))
        .pr(px_len(Spacing::XXS))
        .cursor_pointer()
        .rounded(px_len(Radii::SM))
        .bg(if selected {
            Colors::bg_selected(&theme)
        } else {
            no_fill()
        })
        .hover(|s| s.fx_background(Colors::bg_hover(&theme)))
        .on_click({
            let state = state.clone();
            move |_, _window, cx| {
                state.update(cx, |state, cx| {
                    state.select_project(project_id);
                    state.select_agent(agent_id);
                    // Read the transcript on first selection rather than at launch.
                    state.conversation(project_id, agent_id);
                    cx.notify();
                });
            }
        })
        .child(activity_dot(
            if busy || selected {
                Colors::accent(&theme)
            } else {
                Colors::fg_tertiary(&theme)
            },
            if busy {
                ActivityState::Running
            } else {
                ActivityState::Idle
            },
            0.0,
        ))
        .child(
            div()
                .debug_selector(|| "sidebar-thread-title".into())
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .fx_caption()
                .text_color(if selected {
                    Colors::fg(&theme)
                } else {
                    Colors::fg_secondary(&theme)
                })
                .child(SharedString::from(title)),
        )
        .child(icon_button(
            IconButtonProps::new(Icon::ELLIPSIS_CIRCLE, "Thread actions")
                .tiny()
                .ghost(),
            move |event, _, cx| {
                cx.stop_propagation();
                let layer = state.read(cx).menu_layer.clone();
                layer.update(cx, |layer, cx| {
                    let owner = format!("thread:{agent_id}");
                    if layer.is_owner(&owner) {
                        layer.dismiss();
                    } else {
                        layer.open_at(menu.clone(), owner, event.position());
                    }
                    cx.notify();
                });
            },
        ));
    context_region(
        &registry,
        format!("thread:{agent_id}"),
        Some(context_menu),
        row,
    )
    .into_any_element()
}

fn thread_title(agent: &Agent) -> String {
    let title = agent.title.trim();
    if title.is_empty() {
        "Untitled thread".to_string()
    } else {
        title.to_string()
    }
}

/// The actions for a project row.
fn project_menu(name: &str, busy: bool) -> MenuModel {
    MenuModel::new(vec![
        MenuSection::new(
            "actions",
            vec![
                MenuItem::new("new-thread", "New thread"),
                MenuItem::new("refresh-threads", "Refresh provider threads"),
                MenuItem::new("reveal", "Reveal in Finder").subtitle(name),
            ],
        ),
        MenuSection::new(
            "danger",
            vec![
                MenuItem::new("collapse", "Collapse all threads"),
                MenuItem::new("remove", "Remove from sidebar")
                    .tone(MenuItemTone::Destructive)
                    .enabled(!busy),
            ],
        ),
    ])
}

fn thread_menu(archived: bool, busy: bool) -> MenuModel {
    MenuModel::new(vec![
        MenuSection::new(
            "actions",
            vec![
                MenuItem::new("rename", "Rename thread…").enabled(!busy),
                if archived {
                    MenuItem::new("unarchive", "Restore thread").enabled(!busy)
                } else {
                    MenuItem::new("archive", "Archive thread").enabled(!busy)
                },
            ],
        ),
        MenuSection::new(
            "danger",
            vec![
                MenuItem::new("delete", "Delete thread…")
                    .tone(MenuItemTone::Destructive)
                    .enabled(!busy),
            ],
        ),
    ])
}

/// A thread's title, falling back to a placeholder rather than an empty row.
pub fn fallback_title() -> &'static str {
    "Untitled thread"
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_design::components::{context_frame, handle_root_right_click};
    use fx_store::{AppPaths, PersistedApp, PersistedProject, Store};
    use gpui::{Context, MouseButton, Render, TestAppContext, VisualTestContext, point, size};

    struct SidebarProbe(Entity<AppState>);

    impl Render for SidebarProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let registry = self.0.read(cx).context_registry.clone();
            let click_registry = registry.clone();
            let layer = self.0.read(cx).menu_layer.clone();
            context_frame(
                &registry,
                div()
                    .size_full()
                    .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                        handle_root_right_click(&click_registry, &layer, event, window, cx);
                    })
                    .child(view(self.0.clone(), window, cx)),
            )
        }
    }

    fn seeded(
        test: &mut TestAppContext,
        paths: AppPaths,
        count: usize,
        archive_last: bool,
    ) -> (Entity<AppState>, Vec<Uuid>) {
        let agents: Vec<_> = (0..count)
            .map(|index| Agent::new(format!("Thread {index}")))
            .collect();
        let ids: Vec<_> = agents.iter().map(|agent| agent.id).collect();
        let mut project = Project::new(paths.root().to_string_lossy());
        project.agent_order = ids.clone();
        let snapshot = PersistedApp {
            active_project_id: Some(project.id),
            active_agent_id: ids.first().copied(),
            projects: vec![PersistedProject {
                project: Some(project),
                agents,
                archived_agent_ids: if archive_last {
                    ids.last().copied().into_iter().collect()
                } else {
                    Vec::new()
                },
                is_expanded: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        Store::new(paths.clone()).save_app(&snapshot).unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        (state, ids)
    }

    fn thread_bounds(
        test: &TestAppContext,
        state: &Entity<AppState>,
        id: Uuid,
    ) -> gpui::Bounds<gpui::Pixels> {
        test.read(|cx| {
            state
                .read(cx)
                .context_registry
                .read(cx)
                .regions()
                .iter()
                .find(|region| region.owner.as_ref() == format!("thread:{id}"))
                .expect("painted thread is registered")
                .bounds
        })
    }

    #[test]
    fn header_project_and_thread_actions_share_trailing_columns() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, _) = seeded(&mut test, AppPaths::at(temp.path()), 2, true);
        let for_view = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1100.0), px(700.0)));
        test.refresh().unwrap();

        let workspace = visual
            .debug_bounds("icon-button:Workspace actions")
            .unwrap();
        let add = visual.debug_bounds("icon-button:Add project").unwrap();
        let project = visual.debug_bounds("icon-button:Project actions").unwrap();
        let new_thread = visual.debug_bounds("icon-button:New thread").unwrap();
        let thread = visual.debug_bounds("icon-button:Thread actions").unwrap();
        assert_eq!(workspace.center().x, project.center().x);
        assert_eq!(workspace.center().x, thread.center().x);
        assert_eq!(add.center().x, new_thread.center().x);

        let project_title = visual.debug_bounds("sidebar-project-title").unwrap();
        let active_title = visual.debug_bounds("sidebar-thread-title").unwrap();
        assert!(active_title.left() > project_title.left());

        let toggle = visual
            .debug_bounds("icon-button:Toggle sidebar · ⌘B")
            .unwrap();
        // The sidebar has a half-point column border; the titlebar spans it.
        assert!((toggle.center().x - workspace.center().x).abs() <= px(BorderWidth::HAIRLINE));
        let title = visual.debug_bounds("workspace-title").unwrap();
        assert!(title.left() >= px(Layout::SIDEBAR_WIDTH));

        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.toggle_sidebar();
                cx.notify();
            })
        });
        test.refresh().unwrap();
        let toggle = visual
            .debug_bounds("icon-button:Toggle sidebar · ⌘B")
            .unwrap();
        assert!(toggle.left() >= px(crate::TRAFFIC_LIGHT_GUTTER));
        let title = visual.debug_bounds("workspace-title").unwrap();
        assert!(title.left() < px(Layout::SIDEBAR_WIDTH));
    }

    #[test]
    fn painted_right_click_and_ellipsis_target_the_clicked_thread_without_selecting_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, AppPaths::at(temp.path()), 2, false);
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        test.refresh().unwrap();
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        let bounds = thread_bounds(&test, &state, ids[1]);
        visual.simulate_mouse_down(bounds.center(), MouseButton::Right, Default::default());
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.snapshot.active_agent_id, Some(ids[0]));
            assert_eq!(
                state.menu_layer.read(cx).owner(),
                Some(format!("thread:{}", ids[1]).as_str())
            );
        });
        test.update(|cx| {
            state
                .read(cx)
                .menu_layer
                .clone()
                .update(cx, |layer, _| layer.dismiss());
        });
        test.refresh().unwrap();
        // The trailing action target remains available without a secondary
        // mouse button and must not bubble into the row's selection handler.
        let button = point(bounds.right() - px(20.0), bounds.center().y);
        visual.simulate_mouse_down(button, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(button, MouseButton::Left, Default::default());
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.snapshot.active_agent_id, Some(ids[0]));
            assert_eq!(
                state.menu_layer.read(cx).owner(),
                Some(format!("thread:{}", ids[1]).as_str())
            );
        });
    }

    #[test]
    fn scrolling_clips_regions_and_collapsed_or_hidden_rows_leave_no_stale_targets() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, AppPaths::at(temp.path()), 30, false);
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(Layout::SIDEBAR_WIDTH), px(157.0)));
        test.refresh().unwrap();
        test.read(|cx| {
            let registry = state.read(cx).context_registry.read(cx);
            assert!(registry.len() > 2);
            assert!(registry.len() < ids.len());
            assert!(registry.regions().iter().all(|region| {
                !region.bounds.is_empty() && region.bounds.bottom() <= px(157.0)
            }));
            // At least one laid-out row extends beyond the viewport, but only
            // its painted fragment may offer actions.
            assert!(registry.regions().iter().any(|region| {
                region.owner.starts_with("thread:")
                    && region.bounds.size.height < px(THREAD_ROW_HEIGHT)
            }));
            assert!(
                registry
                    .regions()
                    .iter()
                    .all(|region| region.owner.as_ref() != format!("thread:{}", ids[29]))
            );
        });
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: point(px(100.0), px(120.0)),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(-120.0))),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        test.refresh().unwrap();
        test.read(|cx| {
            let registry = state.read(cx).context_registry.read(cx);
            assert!(!registry.is_empty());
            assert!(registry.regions().iter().all(|region| {
                region.bounds.top() >= px(SIDEBAR_HEADER_HEIGHT)
                    && region.bounds.bottom() <= px(157.0)
                    && region.owner.as_ref() != format!("thread:{}", ids[0])
            }));
        });
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.snapshot.projects[0].is_expanded = false
            });
        });
        test.refresh().unwrap();
        test.read(|cx| assert_eq!(state.read(cx).context_registry.read(cx).len(), 1));
        test.update(|cx| state.update(cx, |state, _| state.sidebar_visible = false));
        test.refresh().unwrap();
        test.read(|cx| assert!(state.read(cx).context_registry.read(cx).is_empty()));
    }

    #[test]
    fn archived_threads_never_render_or_register_sidebar_menus_until_restored() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, AppPaths::at(temp.path()), 2, true);
        let for_view = state.clone();
        test.add_window(move |_, _| SidebarProbe(for_view));
        test.refresh().unwrap();
        test.read(|cx| {
            let registry = state.read(cx).context_registry.read(cx);
            assert!(
                registry
                    .regions()
                    .iter()
                    .any(|region| region.owner.as_ref() == format!("thread:{}", ids[0]))
            );
            assert!(
                !registry
                    .regions()
                    .iter()
                    .any(|region| region.owner.as_ref() == format!("thread:{}", ids[1]))
            );
        });
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.set_thread_archived(ids[1], false));
                cx.notify();
            })
        });
        test.refresh().unwrap();
        test.read(|cx| {
            assert!(
                state
                    .read(cx)
                    .context_registry
                    .read(cx)
                    .regions()
                    .iter()
                    .any(|region| region.owner.as_ref() == format!("thread:{}", ids[1]))
            )
        });
    }

    #[test]
    fn running_threads_cannot_be_renamed_archived_or_deleted() {
        for archived in [false, true] {
            let menu = thread_menu(archived, true);
            assert!(
                menu.sections()
                    .iter()
                    .flat_map(|s| &s.items)
                    .all(|i| !i.enabled)
            );
        }
    }
}
