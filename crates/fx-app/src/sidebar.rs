//! The sidebar: projects and their threads.

use std::collections::HashMap;

use fx_core::{Agent, Project};
use fx_design::components::{
    ButtonStyle, DividerAxis, IconButtonProps, MenuItem, MenuItemTone, MenuModel, MenuSection,
    button_with_icon, collapsible, context_region, divider_with_axis, empty_state, icon_button,
    provider_status,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, global};
use fx_store::PersistedProject;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, MouseButton, Render, Rgba, SharedString,
    Window, div, px,
};
use uuid::Uuid;

use crate::state::AppState;
use crate::text_field::TextField;

gpui::actions!(flowx, [ShowProjectInFinder, CopyProjectPath]);

/// Threads a project lists before "Show more", and how many each press adds.
///
/// The Swift original's page: enough for a day's work, few enough that a
/// project with a long provider history does not open as a wall of rows.
const THREAD_PAGE: usize = 24;

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

    view_at_width(state.clone(), state.read(cx).sidebar_width(), cx)
}

/// A fixed-width body inside the shell's animated clipping region.
pub(crate) fn view_at_width(state: Entity<AppState>, width: f32, cx: &mut App) -> AnyElement {
    let theme = global::current();
    div()
        .w(px(width))
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

fn header(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let theme = global::current();
    let for_state = state.clone();
    // Searching nothing is not an affordance, so an empty workspace keeps the
    // column's name where the search box would be.
    let search = (!state.read(cx).snapshot.projects.is_empty())
        .then(|| state.read(cx).sidebar.search().clone());
    div()
        .h(px(SIDEBAR_HEADER_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .px(px_len(Spacing::MD))
        .gap(px_len(Spacing::XXS))
        .child(match search {
            Some(search) => div().flex_1().min_w_0().child(search).into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from("Projects"))
                .into_any_element(),
        })
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
    let query = state.read(cx).sidebar.search().read(cx).query(cx);
    for record in &snapshot.projects {
        let Some(project) = record.project.as_ref() else {
            continue;
        };
        let id = project.id;
        let selected = Some(id) == snapshot.active_project_id;
        // A search looks through every project, so it opens the collapsed
        // ones for as long as it runs without changing what they reopen as.
        let expanded = record.is_expanded || query.is_active();
        let mut group = div()
            .debug_selector(move || format!("sidebar-project-group:{id}"))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .child(project_row(
                state.clone(),
                id,
                project.clone(),
                selected,
                expanded,
                cx,
            ));
        let mut content = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px_len(Spacing::XXXS))
            .pt(px_len(Spacing::XXXS));
        {
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
                content = content.child(
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
                            // Not tied to whether a background pass is running:
                            // that changes every few seconds, so the button
                            // blinked. A retry while one runs just makes the
                            // next pass due at once.
                            .child(icon_button(
                                IconButtonProps::new(Icon::ARROW_CLOCKWISE, "Retry thread sync")
                                    .tiny()
                                    .ghost()
                                    .enabled(record.is_expanded),
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
                content = content.child(threads(
                    &state,
                    record,
                    agents,
                    &query,
                    snapshot.active_agent_id,
                    expanded,
                    cx,
                ));
            }
        }
        group = group.child(collapsible(
            format!("project-content:{id}"),
            expanded,
            content,
        ));
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
    let menu = project_menu(&project.root_path, busy);
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
                .small()
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
            IconButtonProps::new(Icon::ELLIPSIS, "Project actions")
                .small()
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

/// A project's threads, indented under the project name: those matching the
/// search, one page at a time.
fn threads(
    state: &Entity<AppState>,
    record: &PersistedProject,
    agents: Vec<Agent>,
    query: &ThreadQuery,
    active_agent: Option<Uuid>,
    interactive: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    let project_id = record.project.as_ref().map_or_else(Uuid::nil, |p| p.id);
    let column = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px_len(Spacing::XXXS))
        .pb(px_len(Spacing::XXS));
    if agents.is_empty() {
        return column
            .child(
                div()
                    .pl(px_len(Spacing::XXL))
                    .py(px_len(Spacing::SM))
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child("No threads yet"),
            )
            .into_any_element();
    }
    let matching: Vec<Agent> = if query.is_active() {
        agents
            .into_iter()
            .filter(|agent| query.matches(&search_fields(state.read(cx), agent)))
            .collect()
    } else {
        agents
    };
    if matching.is_empty() {
        return column
            .child(
                div()
                    .debug_selector(move || format!("sidebar-no-matches:{project_id}"))
                    .flex()
                    .flex_col()
                    .gap(px_len(Spacing::XXXS))
                    .pl(px_len(Spacing::XXL))
                    .py(px_len(Spacing::SM))
                    .child(
                        div()
                            .fx_role(TextRole::CaptionMedium)
                            .text_color(Colors::fg_secondary(&theme))
                            .child("No matching threads"),
                    )
                    .child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_tertiary(&theme))
                            .child("Try a title, prompt, or provider."),
                    ),
            )
            .into_any_element();
    }
    let limit = state.read(cx).sidebar.visible_limit(project_id, query);
    let remaining = matching.len().saturating_sub(limit);
    let mut column = column;
    for agent in matching.into_iter().take(limit) {
        let unseen = record.unseen_completion_ids.contains(&agent.id);
        column = column.child(thread_row(
            state.clone(),
            project_id,
            agent.clone(),
            Some(agent.id) == active_agent,
            unseen,
            interactive,
            cx,
        ));
    }
    if remaining > 0 {
        column = column.child(show_more_row(
            state.clone(),
            project_id,
            query.clone(),
            remaining,
            interactive,
        ));
    }
    column.into_any_element()
}

/// What a thread search looks through.
///
/// The title, the provider's name, and the first prompt when the transcript is
/// loaded. A transcript is never read from disk for a search: a keystroke must
/// not parse every thread in every project.
fn search_fields(state: &AppState, agent: &Agent) -> Vec<String> {
    let mut fields = vec![
        thread_title(agent),
        crate::thread_presentation::provider(state, agent)
            .label()
            .to_string(),
    ];
    if let Some(conversation) = state.conversation_by_agent(agent.id) {
        let conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(prompt) = conversation
            .messages()
            .find(|message| message.role == fx_core::MessageRole::User)
        {
            let text = prompt.text_content();
            fields.push(
                fx_core::policy::transcript_presentation::user_prompt(&text)
                    .text
                    .to_owned(),
            );
        }
    }
    fields
}

/// The row under a long list that reveals its next page.
fn show_more_row(
    state: Entity<AppState>,
    project_id: Uuid,
    query: ThreadQuery,
    remaining: usize,
    interactive: bool,
) -> AnyElement {
    let theme = global::current();
    div()
        .id(SharedString::from(format!(
            "show-more-threads:{project_id}"
        )))
        .debug_selector(move || format!("sidebar-show-more:{project_id}"))
        .h(px(THREAD_ROW_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM))
        // A thread row's insets, so the label starts where titles do.
        .pl(px_len(Spacing::SM))
        .pr(px_len(Spacing::SM))
        .cursor_pointer()
        .rounded(px_len(Radii::SM))
        .hover(|s| s.fx_background(Colors::bg_hover(&theme)))
        .on_click(move |_, _, cx| {
            if !interactive {
                return;
            }
            state.update(cx, |state, cx| {
                state.sidebar.show_more(project_id, &query);
                cx.notify();
            });
        })
        .child(
            // As wide as a row's status tile.
            div()
                .w(px_len(Spacing::XL))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(Icon::CHEVRON_DOWN.render(
                    IconSize::Micro,
                    Colors::fg_tertiary(&theme),
                    &theme,
                )),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_nowrap()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from(format!(
                    "Show {} more",
                    remaining.min(THREAD_PAGE)
                ))),
        )
        .child(
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from(format!("{remaining} remaining"))),
        )
        .into_any_element()
}

fn thread_row(
    state: Entity<AppState>,
    project_id: Uuid,
    agent: Agent,
    selected: bool,
    unseen: bool,
    interactive: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    let agent_id = agent.id;
    let title = thread_title(&agent);
    let busy = state.read(cx).is_thread_busy(agent_id);
    let provider = crate::thread_presentation::provider(state.read(cx), &agent);
    let conversation = state.read(cx).conversation_by_agent(agent_id);
    let (activity, activity_label) = {
        let conversation = conversation.as_ref().map(|conversation| {
            conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        });
        crate::thread_presentation::activity(
            conversation
                .as_ref()
                .map(|conversation| conversation.phase()),
            busy,
            agent.execution_state,
        )
    };
    let unseen = crate::thread_presentation::shows_unseen_completion(unseen, selected, activity);
    let thread_id = crate::thread_presentation::provider_thread_id(state.read(cx), agent_id);
    let menu = thread_menu(false, busy, thread_id.as_deref());
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
                if !interactive {
                    return;
                }
                state.update(cx, |state, cx| {
                    state.select_project(project_id);
                    state.select_agent(agent_id);
                    // Read the transcript on first selection rather than at
                    // launch, and in the background rather than in the click.
                    state.request_conversation(project_id, agent_id, cx);
                    cx.notify();
                });
            }
        })
        .child(provider_status(
            format!("thread-status:{agent_id}"),
            provider,
            activity,
            activity_label,
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
                .text_color(if selected || unseen {
                    Colors::fg(&theme)
                } else {
                    Colors::fg_secondary(&theme)
                })
                .child(SharedString::from(title)),
        )
        // A finished turn the reader has not opened yet. Beside the title
        // rather than on the tile, which already says how the turn ended.
        .when(unseen, |row| {
            row.child(
                div()
                    .debug_selector(move || format!("sidebar-unseen:{agent_id}"))
                    .size(px_len(Spacing::XS))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(Colors::accent(&theme)),
            )
        })
        .child(icon_button(
            IconButtonProps::new(Icon::ELLIPSIS, "Thread actions")
                .small()
                .ghost()
                .enabled(interactive),
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
    if !interactive {
        return row.into_any_element();
    }
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
fn project_menu(root_path: &str, busy: bool) -> MenuModel {
    MenuModel::new(vec![
        MenuSection::new(
            "actions",
            vec![
                MenuItem::new("new-thread", "New thread"),
                MenuItem::new("refresh-threads", "Refresh provider threads"),
            ],
        ),
        MenuSection::new(
            "location",
            vec![
                MenuItem::new("reveal", "Show in Finder"),
                // The subtitle is what lands on the clipboard, so the reader
                // can see which folder this is before copying it.
                MenuItem::new("copy-path", "Copy path").subtitle(abbreviated_path(root_path)),
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

/// The actions for a thread row. `thread_id` is the provider's own id, offered
/// for copying once the provider has assigned one.
fn thread_menu(archived: bool, busy: bool, thread_id: Option<&str>) -> MenuModel {
    let mut sections = vec![MenuSection::new(
        "actions",
        vec![
            MenuItem::new("rename", "Rename thread…").enabled(!busy),
            if archived {
                MenuItem::new("unarchive", "Restore thread").enabled(!busy)
            } else {
                MenuItem::new("archive", "Archive thread").enabled(!busy)
            },
        ],
    )];
    if let Some(thread_id) = thread_id {
        // Reading an id changes nothing, so a running turn does not disable it.
        sections.push(MenuSection::new(
            "info",
            vec![MenuItem::new("copy-thread-id", "Copy provider thread ID").subtitle(thread_id)],
        ));
    }
    sections.push(MenuSection::new(
        "danger",
        vec![
            MenuItem::new("delete", "Delete thread…")
                .tone(MenuItemTone::Destructive)
                .enabled(!busy),
        ],
    ));
    MenuModel::new(sections)
}

/// Act on the sidebar's menu rows that write to the clipboard, which
/// `AppState::apply_menu_choice` cannot reach. Returns whether `chosen` was
/// one of them.
pub fn dispatch_menu_choice(
    state: &Entity<AppState>,
    owner: &str,
    chosen: &str,
    cx: &mut App,
) -> bool {
    let id = |prefix: &str| {
        owner
            .strip_prefix(prefix)
            .and_then(|id| Uuid::parse_str(id).ok())
    };
    let text = match chosen {
        "copy-path" => id("project:")
            .and_then(|project| state.read(cx).project(project))
            .map(|project| project.root_path.clone()),
        "copy-thread-id" => id("thread:").and_then(|agent| {
            crate::thread_presentation::provider_thread_id(state.read(cx), agent)
        }),
        _ => return false,
    };
    if let Some(text) = text {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
    true
}

/// Handle the File menu's project commands on `root`, while a project is
/// selected.
///
/// Only then, because GPUI enables a menu-bar item exactly when something on
/// the focused path handles its action. The Swift original hid both commands
/// without a project; a static GPUI menu can only grey them out.
pub fn project_commands<E: InteractiveElement>(root: E, state: &Entity<AppState>, cx: &App) -> E {
    if state.read(cx).active_project().is_none() {
        return root;
    }
    let reveal = state.clone();
    let copy = state.clone();
    root.on_action(move |_: &ShowProjectInFinder, _, cx| {
        if let Some(project) = reveal.read(cx).active_project() {
            let path = std::path::PathBuf::from(&project.root_path);
            cx.reveal_path(&path);
        }
    })
    .on_action(move |_: &CopyProjectPath, _, cx| {
        if let Some(project) = copy.read(cx).active_project() {
            let path = project.root_path.clone();
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    })
}

/// A path as the reader would write it, with the home folder as `~`.
fn abbreviated_path(path: &str) -> String {
    abbreviated_under(path, std::env::var("HOME").ok().as_deref())
}

fn abbreviated_under(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|home| home.trim_end_matches('/'))
        .filter(|home| !home.is_empty())
    else {
        return path.to_string();
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        // Only at a component boundary: `/Users/ann` is not home to `/Users/anna`.
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// A thread's title, falling back to a placeholder rather than an empty row.
pub fn fallback_title() -> &'static str {
    "Untitled thread"
}

/// Sidebar state that outlives a frame but not the session: the thread search
/// and how far each project's list has been paged.
pub struct SidebarUi {
    search: Entity<SidebarSearch>,
    /// Pages revealed beyond the first, per project, for `paged_query` only.
    pages: HashMap<Uuid, usize>,
    paged_query: String,
}

impl SidebarUi {
    pub fn new(cx: &mut App) -> Self {
        Self {
            search: cx.new(SidebarSearch::new),
            pages: HashMap::new(),
            paged_query: String::new(),
        }
    }

    pub fn search(&self) -> &Entity<SidebarSearch> {
        &self.search
    }

    /// How many matching threads a project lists for `query`.
    ///
    /// A different query starts every project over at one page: rows revealed
    /// while looking for one thing say nothing about how many the next search
    /// needs, and keeping them would hide the "Show more" row it does need.
    pub fn visible_limit(&self, project: Uuid, query: &ThreadQuery) -> usize {
        let pages = if self.paged_query == query.key() {
            self.pages.get(&project).copied().unwrap_or_default()
        } else {
            0
        };
        THREAD_PAGE * (pages + 1)
    }

    /// Reveal one more page of a project's matches for `query`.
    pub fn show_more(&mut self, project: Uuid, query: &ThreadQuery) {
        let key = query.key();
        if self.paged_query != key {
            self.pages.clear();
            self.paged_query = key;
        }
        *self.pages.entry(project).or_default() += 1;
    }
}

/// The search box in the sidebar's header.
///
/// A view of its own rather than part of the sidebar's element tree, because
/// its focus ring needs the window, which the sidebar is built without. The
/// field inside is bare so the magnifier and the clear button sit within one
/// outline, as the Swift original drew them.
pub struct SidebarSearch {
    field: Entity<TextField>,
}

impl SidebarSearch {
    fn new(cx: &mut Context<Self>) -> Self {
        let field = cx.new(|cx| {
            TextField::new(cx)
                .bare()
                .placeholder("Search threads")
                .max_len(200)
        });
        Self { field }
    }

    pub fn field(&self) -> &Entity<TextField> {
        &self.field
    }

    pub fn query(&self, cx: &App) -> ThreadQuery {
        ThreadQuery::parse(self.field.read(cx).value())
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.field
            .update(cx, |field, cx| field.set_value(String::new(), cx));
        cx.notify();
    }
}

impl Render for SidebarSearch {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let focus = self.field.read(cx).focus_handle();
        let focused = focus.is_focused(window);
        let empty = self.field.read(cx).value().is_empty();
        // A text field's own rule: focus thickens the border, and the padding
        // gives the difference back so the text does not move.
        let border = if focused {
            BorderWidth::REGULAR
        } else {
            BorderWidth::HAIRLINE
        };
        let this = cx.entity();
        let for_escape = this.clone();
        let for_clear = focus.clone();
        div()
            .id("sidebar-search")
            .debug_selector(|| "sidebar-search".into())
            .h(px(Layout::CONTROL_COMPACT_HEIGHT))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px_len(Spacing::XS))
            .pl(px(Spacing::SM - border))
            .pr(px(Spacing::XS - border))
            .rounded(px_len(Radii::MD))
            .bg(Colors::input_bg(&theme))
            .border(px(border))
            .border_color(if focused {
                Colors::accent(&theme)
            } else {
                Colors::border_subtle(&theme)
            })
            .cursor_text()
            // The magnifier and the padding are part of the box, so a click
            // there means the field as much as a click on the text does.
            .on_mouse_down(MouseButton::Left, move |_, window, _| focus.focus(window))
            .on_key_down(move |event, _, cx| {
                if event.keystroke.key == "escape"
                    && !for_escape.read(cx).field.read(cx).value().is_empty()
                {
                    cx.stop_propagation();
                    for_escape.update(cx, |search, cx| search.clear(cx));
                }
            })
            .child(Icon::MAGNIFYINGGLASS.render(
                IconSize::Small,
                Colors::fg_tertiary(&theme),
                &theme,
            ))
            .child(div().flex_1().min_w_0().child(self.field.clone()))
            .when(!empty, |row| {
                row.child(
                    div()
                        .id("sidebar-search-clear")
                        .debug_selector(|| "sidebar-search-clear".into())
                        .flex_shrink_0()
                        .cursor_pointer()
                        .tooltip(|window, cx| {
                            fx_design::components::tooltip("Clear search", window, cx)
                        })
                        .on_click(move |_, window, cx| {
                            this.update(cx, |search, cx| search.clear(cx));
                            for_clear.focus(window);
                        })
                        .child(Icon::XMARK_CIRCLE_FILL.render(
                            IconSize::Small,
                            Colors::fg_tertiary(&theme),
                            &theme,
                        )),
                )
            })
    }
}

/// A parsed thread search: whitespace-separated terms, every one of which must
/// appear somewhere in a thread's searchable text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadQuery {
    terms: Vec<String>,
}

impl ThreadQuery {
    pub fn parse(raw: &str) -> Self {
        Self {
            terms: fold(raw).split_whitespace().map(str::to_owned).collect(),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.terms.is_empty()
    }

    /// The query as typed, minus case, accents and spacing, so pagination
    /// survives an edit that does not change what is being searched for.
    fn key(&self) -> String {
        self.terms.join(" ")
    }

    /// Whether every term appears in one of `fields`. An empty query matches.
    pub fn matches(&self, fields: &[String]) -> bool {
        let fields: Vec<String> = fields.iter().map(|field| fold(field)).collect();
        self.terms
            .iter()
            .all(|term| fields.iter().any(|field| field.contains(term.as_str())))
    }
}

/// Lower-case `text` and drop the accents Latin scripts put on a base letter,
/// so "cafe" finds "Café" and "acao" finds "Ação".
///
/// A table rather than Unicode decomposition, because the workspace has no
/// normalisation crate. Combining marks are dropped as well, which covers text
/// already decomposed — file names on macOS often are.
fn fold(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .map(|c| match c {
            'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
            'ď' | 'đ' => 'd',
            'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
            'ĥ' | 'ħ' => 'h',
            'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
            'ĵ' => 'j',
            'ķ' => 'k',
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
            'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
            'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
            'ŕ' | 'ŗ' | 'ř' => 'r',
            'ś' | 'ŝ' | 'ş' | 'š' => 's',
            'ţ' | 'ť' | 'ŧ' => 't',
            'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
            'ŵ' => 'w',
            'ý' | 'ÿ' | 'ŷ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        })
        .collect()
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
        window
            .update(&mut test, |view, _, cx| {
                view.sidebar_transition =
                    Some(fx_design::theme::animation::PanelTransition::new(false));
                cx.notify();
            })
            .unwrap();
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
        test.read(|cx| {
            assert!(
                state
                    .read(cx)
                    .context_registry
                    .read(cx)
                    .regions()
                    .iter()
                    .all(|region| { !region.owner.starts_with("thread:") })
            );
        });
        std::thread::sleep(fx_design::theme::animation::FxAnimation::PANEL);
        test.refresh().unwrap();
        test.update(|_| {});
        test.read(|cx| assert_eq!(state.read(cx).context_registry.read(cx).len(), 1));
        test.update(|cx| state.update(cx, |state, _| state.sidebar_visible = false));
        test.refresh().unwrap();
        test.update(|_| {});
        test.read(|cx| assert!(state.read(cx).context_registry.read(cx).is_empty()));
    }

    #[test]
    fn project_disclosure_animates_its_measured_height_and_restores_visible_targets() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, _) = seeded(&mut test, AppPaths::at(temp.path()), 3, false);
        let project_id = test.read(|cx| state.read(cx).snapshot.active_project_id.unwrap());
        let selector: &'static str =
            Box::leak(format!("sidebar-project-group:{project_id}").into_boxed_str());
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        test.refresh().unwrap();
        test.update(|_| {});
        let full_height = visual.debug_bounds(selector).unwrap().size.height;
        assert!(full_height >= px(PROJECT_ROW_HEIGHT + THREAD_ROW_HEIGHT * 3.0));

        test.update(|cx| {
            state.update(cx, |state, _| state.toggle_project_expansion(project_id));
        });
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(visual.debug_bounds(selector).unwrap().size.height > px(PROJECT_ROW_HEIGHT));
        test.read(|cx| assert_eq!(state.read(cx).context_registry.read(cx).len(), 1));
        std::thread::sleep(fx_design::theme::animation::FxAnimation::PANEL);
        test.refresh().unwrap();
        test.update(|_| {});
        assert_eq!(
            visual.debug_bounds(selector).unwrap().size.height,
            px(PROJECT_ROW_HEIGHT)
        );

        test.update(|cx| {
            state.update(cx, |state, _| state.toggle_project_expansion(project_id));
        });
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(visual.debug_bounds(selector).unwrap().size.height < full_height);
        std::thread::sleep(fx_design::theme::animation::FxAnimation::PANEL);
        test.refresh().unwrap();
        test.update(|_| {});
        assert_eq!(
            visual.debug_bounds(selector).unwrap().size.height,
            full_height
        );
        test.read(|cx| assert_eq!(state.read(cx).context_registry.read(cx).len(), 4));
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
    fn running_threads_cannot_be_renamed_archived_or_deleted_but_can_copy_their_id() {
        for archived in [false, true] {
            let menu = thread_menu(archived, true, Some("thread-123"));
            for item in menu.sections().iter().flat_map(|s| &s.items) {
                assert_eq!(item.enabled, item.id == "copy-thread-id", "{}", item.id);
            }
        }
    }

    fn item<'a>(menu: &'a MenuModel, id: &str) -> Option<&'a MenuItem> {
        menu.sections()
            .iter()
            .flat_map(|section| &section.items)
            .find(|item| item.id == id)
    }

    fn binding(agent_id: Uuid, session_id: &str) -> fx_store::PersistedBinding {
        fx_store::PersistedBinding {
            agent_id,
            provider_id: "codex".into(),
            session_id: session_id.into(),
            title: String::new(),
            working_directory: String::new(),
            updated_at: fx_core::models::now(),
            model: None,
            effort: None,
            agent_access: None,
            is_archived: false,
            revision: String::new(),
            context_window_size: None,
            local_history_pending: false,
            local_start: None,
        }
    }

    fn clipboard(test: &TestAppContext) -> Option<String> {
        test.read_from_clipboard().and_then(|item| item.text())
    }

    #[test]
    fn project_and_thread_menus_copy_the_path_and_provider_id_they_show() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let paths = AppPaths::at(temp.path());
        let (state, ids) = seeded(&mut test, paths.clone(), 2, false);
        let project_id = test.read(|cx| state.read(cx).snapshot.active_project_id.unwrap());
        // The second thread was never opened here; only its binding knows it.
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.snapshot.projects[0]
                    .native_bindings
                    .push(binding(ids[1], "codex-thread-7"));
            });
            crate::AppHandle::install(state.clone(), cx);
        });
        let menus = test.read(|cx| {
            let state = state.read(cx);
            let thread = |id| crate::thread_presentation::provider_thread_id(state, id);
            (thread(ids[0]), thread(ids[1]))
        });
        assert_eq!(menus, (None, Some("codex-thread-7".to_string())));
        assert!(
            item(
                &thread_menu(false, false, menus.0.as_deref()),
                "copy-thread-id"
            )
            .is_none()
        );
        assert_eq!(
            item(
                &thread_menu(false, false, menus.1.as_deref()),
                "copy-thread-id"
            )
            .and_then(|item| item.subtitle.as_deref()),
            Some("codex-thread-7")
        );
        let root = paths.root().to_string_lossy().into_owned();
        assert!(item(&project_menu(&root, false), "copy-path").is_some());
        assert!(item(&project_menu(&root, false), "reveal").is_some());

        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let choose = |test: &mut TestAppContext, owner: String, chosen: &'static str| {
            window
                .update(test, |_, window, cx| {
                    crate::dispatch_menu_choice(&owner, chosen, window, cx)
                })
                .unwrap();
        };
        choose(&mut test, format!("project:{project_id}"), "copy-path");
        assert_eq!(clipboard(&test), Some(root));
        choose(&mut test, format!("thread:{}", ids[1]), "copy-thread-id");
        assert_eq!(clipboard(&test).as_deref(), Some("codex-thread-7"));
        // A thread without an id leaves the clipboard as it was.
        choose(&mut test, format!("thread:{}", ids[0]), "copy-thread-id");
        assert_eq!(clipboard(&test).as_deref(), Some("codex-thread-7"));
        // A live session id wins over a binding that has not caught up yet.
        test.update(|cx| {
            state.update(cx, |state, _| {
                state
                    .conversation(project_id, ids[1])
                    .lock()
                    .unwrap()
                    .set_session_id("codex-thread-8");
            })
        });
        choose(&mut test, format!("thread:{}", ids[1]), "copy-thread-id");
        assert_eq!(clipboard(&test).as_deref(), Some("codex-thread-8"));
    }

    #[test]
    fn the_file_menu_project_commands_exist_only_while_a_project_is_selected() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let paths = AppPaths::at(temp.path());
        let (state, _) = seeded(&mut test, paths.clone(), 1, false);
        let project_id = test.read(|cx| state.read(cx).snapshot.active_project_id.unwrap());
        let for_view = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(for_view));
        test.refresh().unwrap();
        let available = |test: &mut TestAppContext| {
            window
                .update(test, |_, window, cx| {
                    (
                        window.is_action_available(&ShowProjectInFinder, cx),
                        window.is_action_available(&CopyProjectPath, cx),
                    )
                })
                .unwrap()
        };
        assert_eq!(available(&mut test), (true, true));
        test.dispatch_action(window.into(), CopyProjectPath);
        assert_eq!(
            clipboard(&test),
            Some(paths.root().to_string_lossy().into_owned())
        );

        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.remove_project(project_id);
                cx.notify();
            })
        });
        assert_eq!(available(&mut test), (false, false));
    }

    #[test]
    fn a_copied_path_is_previewed_relative_to_home_only_inside_it() {
        let home = Some("/Users/ann/");
        assert_eq!(abbreviated_under("/Users/ann", home), "~");
        assert_eq!(abbreviated_under("/Users/ann/code/x", home), "~/code/x");
        assert_eq!(
            abbreviated_under("/Users/anna/code", home),
            "/Users/anna/code"
        );
        assert_eq!(abbreviated_under("/srv/x", home), "/srv/x");
        assert_eq!(abbreviated_under("/srv/x", Some("/")), "/srv/x");
        assert_eq!(abbreviated_under("/srv/x", None), "/srv/x");
    }

    #[test]
    fn search_folds_case_and_accents_and_needs_every_term() {
        let fields = |values: &[&str]| values.iter().map(|v| v.to_string()).collect::<Vec<_>>();
        let title = fields(&["Café com Ação", "Codex"]);
        assert!(ThreadQuery::parse("").matches(&title));
        assert!(!ThreadQuery::parse("   ").is_active());
        assert!(ThreadQuery::parse("CAFE").matches(&title));
        assert!(ThreadQuery::parse("  acao   cafe ").matches(&title));
        // Terms may come from different fields, but each must appear.
        assert!(ThreadQuery::parse("acao codex").matches(&title));
        assert!(!ThreadQuery::parse("acao claude").matches(&title));
        // Decomposed text, as macOS file names often are, folds the same way.
        assert!(ThreadQuery::parse("ação").matches(&fields(&["Ac\u{327}a\u{303}o"])));
        assert_eq!(
            ThreadQuery::parse(" Ação  Café").key(),
            ThreadQuery::parse("acao cafe").key()
        );
    }

    #[test]
    fn paging_is_per_project_and_restarts_for_a_new_query() {
        let test = TestAppContext::single();
        let mut ui = test.update(SidebarUi::new);
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        let all = ThreadQuery::default();
        let search = ThreadQuery::parse("fix");
        assert_eq!(ui.visible_limit(first, &all), THREAD_PAGE);
        ui.show_more(first, &all);
        ui.show_more(first, &all);
        assert_eq!(ui.visible_limit(first, &all), THREAD_PAGE * 3);
        assert_eq!(ui.visible_limit(second, &all), THREAD_PAGE);
        // Pages revealed for one query say nothing about the next one.
        assert_eq!(ui.visible_limit(first, &search), THREAD_PAGE);
        ui.show_more(second, &search);
        assert_eq!(ui.visible_limit(second, &search), THREAD_PAGE * 2);
        assert_eq!(ui.visible_limit(first, &all), THREAD_PAGE);
        // Re-spelling the same search keeps its pages.
        assert_eq!(
            ui.visible_limit(second, &ThreadQuery::parse("  FIX ")),
            THREAD_PAGE * 2
        );
    }

    fn thread_regions(test: &TestAppContext, state: &Entity<AppState>) -> Vec<String> {
        test.read(|cx| {
            state
                .read(cx)
                .context_registry
                .read(cx)
                .regions()
                .iter()
                .filter(|region| region.owner.starts_with("thread:"))
                .map(|region| region.owner.to_string())
                .collect()
        })
    }

    /// A sidebar in a window of its own, tall enough that only paging and not
    /// the viewport limits the rows.
    ///
    /// GPUI never clears a frame's debug bounds, so a selector painted once in
    /// a window stays "found" there. Absence can only be asserted in a window
    /// that has never painted it.
    fn tall_sidebar(
        test: &mut TestAppContext,
        state: &Entity<AppState>,
    ) -> (gpui::WindowHandle<SidebarProbe>, VisualTestContext) {
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let visual = VisualTestContext::from_window(window.into(), test);
        visual.simulate_resize(size(px(Layout::SIDEBAR_WIDTH), px(2400.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        (window, visual)
    }

    #[test]
    fn long_lists_page_and_a_search_reaches_into_collapsed_projects() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, AppPaths::at(temp.path()), 30, false);
        let project_id = test.read(|cx| state.read(cx).snapshot.active_project_id.unwrap());
        let show_more: &'static str =
            Box::leak(format!("sidebar-show-more:{project_id}").into_boxed_str());
        let no_matches: &'static str =
            Box::leak(format!("sidebar-no-matches:{project_id}").into_boxed_str());
        let (window, mut visual) = tall_sidebar(&mut test, &state);
        assert_eq!(thread_regions(&test, &state).len(), THREAD_PAGE);
        assert!(!thread_regions(&test, &state).contains(&format!("thread:{}", ids[THREAD_PAGE])));
        let button = visual.debug_bounds(show_more).expect("a long list pages");
        visual.simulate_click(button.center(), Default::default());
        test.refresh().unwrap();
        assert_eq!(thread_regions(&test, &state).len(), ids.len());
        let (fresh_window, mut fresh) = tall_sidebar(&mut test, &state);
        assert!(
            fresh.debug_bounds(show_more).is_none(),
            "nothing left to show"
        );
        // Both windows paint into the one registry, so only one may remain.
        fresh_window
            .update(&mut test, |_, window, _| window.remove_window())
            .unwrap();
        test.refresh().unwrap();
        test.update(|_| {});

        // A search opens a collapsed project without changing what it reopens as.
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.toggle_project_expansion(project_id);
                cx.notify();
            });
        });
        settle_disclosure(&mut test);
        assert!(thread_regions(&test, &state).is_empty());
        let field = test.read(|cx| state.read(cx).sidebar.search().read(cx).field().clone());
        test.update(|cx| field.update(cx, |field, cx| field.set_value("THREAD 29", cx)));
        settle_disclosure(&mut test);
        assert_eq!(
            thread_regions(&test, &state),
            vec![format!("thread:{}", ids[29])]
        );
        assert!(test.read(|cx| !state.read(cx).is_expanded(project_id)));

        test.update(|cx| field.update(cx, |field, cx| field.set_value("nothing like it", cx)));
        settle_disclosure(&mut test);
        assert!(thread_regions(&test, &state).is_empty());
        assert!(visual.debug_bounds(no_matches).is_some());

        // Escape in the box clears the search, and the project folds back up.
        test.update(|cx| {
            let focus = field.read(cx).focus_handle();
            window
                .update(cx, |_, window, _| focus.focus(window))
                .unwrap();
        });
        test.simulate_keystrokes(window.into(), "escape");
        test.read(|cx| assert_eq!(field.read(cx).value(), ""));
        test.update(|cx| field.update(cx, |field, cx| field.set_value("29", cx)));
        settle_disclosure(&mut test);
        assert_eq!(thread_regions(&test, &state).len(), 1);
        test.simulate_keystrokes(window.into(), "escape");
        settle_disclosure(&mut test);
        assert!(thread_regions(&test, &state).is_empty());
    }

    /// Let a project's disclosure animation run to its end and repaint.
    fn settle_disclosure(test: &mut TestAppContext) {
        test.refresh().unwrap();
        test.update(|_| {});
        std::thread::sleep(fx_design::theme::animation::FxAnimation::PANEL);
        test.refresh().unwrap();
        test.update(|_| {});
    }

    fn finish(state: &Entity<AppState>, test: &mut TestAppContext, agent: Uuid) {
        test.update(|cx| {
            state.update(cx, |state, cx| {
                (state.update_sender_for(agent))(crate::session::SessionUpdate::Finished {
                    outcome: fx_agent::codex::TurnOutcome::Completed {
                        thread_id: "thread".into(),
                        stop_reason: "end_turn".into(),
                    },
                });
                state.drain_updates();
                cx.notify();
            })
        });
        test.refresh().unwrap();
    }

    #[test]
    fn a_turn_finished_out_of_view_is_marked_until_opened_and_across_a_relaunch() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path());
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, paths.clone(), 2, false);
        let dot = |id: Uuid| -> &'static str {
            Box::leak(format!("sidebar-unseen:{id}").into_boxed_str())
        };
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        test.refresh().unwrap();

        // The open thread finishing is already seen.
        finish(&state, &mut test, ids[0]);
        assert!(visual.debug_bounds(dot(ids[0])).is_none());
        finish(&state, &mut test, ids[1]);
        assert!(visual.debug_bounds(dot(ids[1])).is_some());
        let saved = Store::new(paths.clone()).load_app().unwrap();
        assert_eq!(saved.projects[0].unseen_completion_ids, vec![ids[1]]);

        // A relaunch restores the mark from disk.
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        let for_view = state.clone();
        let window = test.add_window(move |_, _| SidebarProbe(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        test.refresh().unwrap();
        assert!(visual.debug_bounds(dot(ids[1])).is_some());

        let row = thread_bounds(&test, &state, ids[1]);
        visual.simulate_click(row.center(), Default::default());
        test.refresh().unwrap();
        test.read(|cx| {
            assert_eq!(state.read(cx).snapshot.active_agent_id, Some(ids[1]));
            assert!(!state.read(cx).has_unseen_completion(ids[1]));
        });
        let saved = Store::new(paths).load_app().unwrap();
        assert!(saved.projects[0].unseen_completion_ids.is_empty());
        // Leaving the opened thread does not bring its mark back.
        test.update(|cx| state.update(cx, |state, _| state.select_agent(ids[0])));
        let (_, mut fresh) = tall_sidebar(&mut test, &state);
        assert!(fresh.debug_bounds(dot(ids[0])).is_none());
        assert!(fresh.debug_bounds(dot(ids[1])).is_none());
    }

    #[test]
    fn a_thread_left_after_the_selection_moved_onto_it_is_not_marked_again() {
        let temp = tempfile::tempdir().unwrap();
        let mut test = TestAppContext::single();
        let (state, ids) = seeded(&mut test, AppPaths::at(temp.path()), 3, false);
        finish(&state, &mut test, ids[1]);
        test.update(|cx| {
            state.update(cx, |state, _| {
                assert!(state.has_unseen_completion(ids[1]));
                // Archiving the open thread moves the selection by itself.
                state.snapshot.projects[0].last_selected_agent_id = Some(ids[1]);
                assert!(state.set_thread_archived(ids[0], true));
                assert_eq!(state.snapshot.active_agent_id, Some(ids[1]));
                state.select_agent(ids[2]);
                assert!(!state.has_unseen_completion(ids[1]));
            })
        });
    }
}
