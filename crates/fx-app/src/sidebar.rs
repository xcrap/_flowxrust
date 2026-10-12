//! The sidebar: projects and their threads.

use fx_core::{Agent, AgentMode, Project};
use fx_design::components::{
    ActivityState, BadgeTone, ButtonStyle, DividerAxis, IconButtonProps, MenuItem, MenuItemTone,
    MenuModel, MenuSection, activity_dot, button_with_icon, divider_with_axis, empty_state,
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
            let agents = project
                .agent_order
                .iter()
                .filter_map(|id| record.agents.iter().find(|agent| agent.id == *id))
                .cloned()
                .collect();
            group = group.child(threads(&state, id, agents, snapshot.active_agent_id, cx));
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
    _cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    let label = if project.name.trim().is_empty() {
        "Untitled project".to_string()
    } else {
        project.name.clone()
    };
    // Built before the closures below, so neither has to borrow the project.
    let menu = project_menu(&project.name);

    div()
        .id(SharedString::from(format!("project:{id}")))
        .h(px(PROJECT_ROW_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::XXS))
        .pl(px_len(Spacing::XS))
        .pr(px_len(Spacing::XXXS))
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
                    layer.open_at(model, format!("project:{id}"), event.position());
                    cx.notify();
                });
            },
        ))
        .into_any_element()
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
    let mode = agent.configuration.resolved_mode();
    let busy = state.read(cx).runners.contains_key(&agent_id);

    div()
        .id(SharedString::from(format!("thread:{agent_id}")))
        .h(px(THREAD_ROW_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM))
        // Past the project row's disclosure triangle and gutter.
        .pl(px_len(Spacing::SM))
        .pr(px_len(Spacing::SM))
        .cursor_pointer()
        .rounded(px_len(Radii::SM))
        .bg(if selected {
            Colors::bg_selected(&theme)
        } else {
            no_fill()
        })
        .hover(|s| s.fx_background(Colors::bg_hover(&theme)))
        .on_click(move |_, _window, cx| {
            state.update(cx, |state, cx| {
                state.select_project(project_id);
                state.select_agent(agent_id);
                // Read the transcript on first selection rather than at launch.
                state.conversation(project_id, agent_id);
                cx.notify();
            });
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
        // Only a plan thread earns a badge; one on every chat thread turns the
        // sidebar into a wall of identical chips.
        .child(if mode == AgentMode::Plan {
            fx_design::components::badge(SharedString::from(mode.label()), BadgeTone::Accent, false)
                .into_any_element()
        } else {
            div().w(px(0.0)).h(px(0.0)).into_any_element()
        })
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
fn project_menu(name: &str) -> MenuModel {
    MenuModel::new(vec![
        MenuSection::new(
            "actions",
            vec![
                MenuItem::new("new-thread", "New thread"),
                MenuItem::new("reveal", "Reveal in Finder").subtitle(name),
            ],
        ),
        MenuSection::new(
            "danger",
            vec![
                MenuItem::new("collapse", "Collapse all threads"),
                MenuItem::new("remove", "Remove from sidebar").tone(MenuItemTone::Destructive),
            ],
        ),
    ])
}

/// A thread's title, falling back to a placeholder rather than an empty row.
pub fn fallback_title() -> &'static str {
    "Untitled thread"
}
