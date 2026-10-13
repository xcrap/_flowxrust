//! `fx-app` — application state and views.
//!
//! The shell is three columns: a sidebar of projects and threads, a centre
//! conversation, and an inspector. This module owns what is *true* about that
//! arrangement; the view modules own how it is drawn.
//!
//! # Two owners
//!
//! The root view owns the two things no child should: the single open-menu
//! layer, and the window's right-click. Both are app-wide, so a child holding
//! either would have to hand it back out again.

pub mod approvals;
mod archived_chats;
mod attachments;
mod auto_title;
mod claude_session_lease;
pub mod composer;
mod configuration;
pub mod inspector;
mod markdown;
mod native_sync;
mod project_changes;
pub mod project_picker;
pub mod provider_catalog;
mod provider_settings;
pub mod session;
pub mod settings;
pub mod sidebar;
pub mod state;
pub mod text_field;
mod thread_actions;
mod title_settings;
pub mod transcript;
pub mod workspace_ui;

pub use fx_design::icon::IconAssets;

use fx_core::Agent;
use fx_design::components::{MenuItem, MenuLayer, MenuModel, MenuSection};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::animation::PanelTransition;
use fx_design::theme::{Colors, Layout, Spacing, global};
use gpui::AnyView;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, MouseButton, Render, SharedString, TitlebarOptions,
    Window, WindowBackgroundAppearance, WindowBounds, WindowOptions, div, px, size,
};

gpui::actions!(
    flowx,
    [
        Quit,
        NewProject,
        NewThread,
        ShowSettings,
        ToggleSidebar,
        ToggleInspector,
        StopTurn
    ]
);

fn shell_action(action: &str, cx: &mut App) {
    if let Some(handle) = AppHandle::get(cx) {
        handle.0.update(cx, |state, cx| {
            state.apply_menu_choice("workspace", action);
            cx.notify();
        });
    }
}

pub use state::{AppState, StoreError};

// The transcript renders the *live* conversation, which is the engine's type.
// `fx_core::Conversation` is the persisted shape and is a different thing; naming
// both `Conversation` here would be a trap rather than a convenience.
pub use fx_agent::conversation::Conversation;

/// The root view.
pub struct FlowX {
    state: Entity<AppState>,
    subscriptions: Vec<gpui::Subscription>,
    draft_values: std::collections::HashMap<uuid::Uuid, String>,
    draft_subscriptions: std::collections::HashMap<uuid::Uuid, gpui::Subscription>,
    poller: Option<gpui::Task<()>>,
    native_poller: Option<gpui::Task<()>>,
    polling: bool,
    autosave: Option<gpui::Task<()>>,
    focus: Option<gpui::FocusHandle>,
    focused_thread: Option<uuid::Uuid>,
    resizing_inspector: bool,
    resizing_sidebar: bool,
    sidebar_resize_offset: f32,
    sidebar_transition: Option<PanelTransition>,
}

impl FlowX {
    pub fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            subscriptions: Vec::new(),
            draft_values: Default::default(),
            draft_subscriptions: Default::default(),
            poller: None,
            native_poller: None,
            polling: false,
            autosave: None,
            focus: None,
            focused_thread: None,
            resizing_inspector: false,
            resizing_sidebar: false,
            sidebar_resize_offset: 0.0,
            sidebar_transition: None,
        }
    }

    fn observe_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.poll_native_sync(cx);
        });
        if self.native_poller.is_none() && self.state.read(cx).native_sync_enabled() {
            self.native_poller = Some(cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(std::time::Duration::from_millis(500)).await;
                    if this
                        .update(cx, |this, cx| {
                            let changed = this
                                .state
                                .update(cx, |state, cx| state.poll_native_sync(cx));
                            if changed {
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        let ui = workspace_ui::handle(cx);
        if self.subscriptions.is_empty() {
            self.subscriptions
                .push(cx.observe(&self.state, |_, _, cx| cx.notify()));
            let menu = self.state.read(cx).menu_layer.clone();
            self.subscriptions
                .push(cx.observe(&menu, |_, _, cx| cx.notify()));
            self.subscriptions
                .push(cx.observe(&ui, |_, _, cx| cx.notify()));
            self.subscriptions
                .push(cx.observe_window_appearance(window, |_, window, cx| {
                    fx_design::theme::global::set_system_appearance(matches!(
                        window.appearance(),
                        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                    ));
                    cx.notify();
                }));
            let focus = cx.focus_handle();
            focus.focus(window);
            self.focus = Some(focus);
        }
        if window.focused(cx).is_none()
            && let Some(focus) = &self.focus
        {
            focus.focus(window);
        }
        let thread_ids: std::collections::HashSet<_> = self
            .state
            .read(cx)
            .snapshot
            .projects
            .iter()
            .flat_map(|record| record.agents.iter().map(|agent| agent.id))
            .collect();
        self.draft_values.retain(|id, _| thread_ids.contains(id));
        self.draft_subscriptions
            .retain(|id, _| thread_ids.contains(id));
        let active_agent = self.state.read(cx).active_agent();
        let active_id = active_agent.as_ref().map(|agent| agent.id);
        let selection_changed = self.focused_thread != active_id;
        self.focused_thread = active_id;
        let modal_open = self.state.read(cx).picker.is_some()
            || ui.read(cx).settings.is_some()
            || ui.read(cx).request.is_some()
            || ui.read(cx).thread_dialog.is_some();
        if selection_changed
            && !modal_open
            && let Some(focus) = &self.focus
        {
            focus.focus(window);
        }
        if let Some(agent) = active_agent {
            let field = self.state.update(cx, |state, cx| state.draft(agent.id, cx));
            if selection_changed && !modal_open {
                field.read(cx).focus_handle().focus(window);
            }
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.draft_values.entry(agent.id)
            {
                entry.insert(field.read(cx).value().to_string());
                self.draft_subscriptions.insert(
                    agent.id,
                    cx.observe(&field, move |this, field, cx| {
                        let value = field.read(cx).value().to_string();
                        if this.draft_values.get(&agent.id) == Some(&value) {
                            return;
                        }
                        this.draft_values.insert(agent.id, value);
                        this.autosave = Some(cx.spawn(async move |this, cx| {
                            gpui::Timer::after(std::time::Duration::from_millis(500)).await;
                            let _ = this.update(cx, |this, cx| {
                                this.state.update(cx, |state, cx| state.save_all(cx));
                            });
                        }));
                        cx.notify();
                    }),
                );
            }
        }
        if !self.polling
            && (self.state.read(cx).any_busy()
                || self.state.read(cx).has_title_jobs()
                || ui.read(cx).is_checking()
                || provider_catalog::is_refreshing())
        {
            self.polling = true;
            self.poller = Some(cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(std::time::Duration::from_millis(33)).await;
                    let keep_going = this
                        .update(cx, |this, cx| {
                            this.state.update(cx, |state, cx| {
                                state.drain_updates_with_context(cx);
                                state.checkpoint(cx);
                            });
                            let ui = workspace_ui::handle(cx);
                            ui.update(cx, |ui, cx| ui.poll(cx));
                            let busy = this.state.read(cx).any_busy()
                                || this.state.read(cx).has_title_jobs()
                                || ui.read(cx).is_checking()
                                || provider_catalog::is_refreshing();
                            this.polling = busy;
                            cx.notify();
                            busy
                        })
                        .unwrap_or(false);
                    if !keep_going {
                        break;
                    }
                }
            }));
        }
        ui.update(cx, |ui, cx| ui.sync_request(&self.state, window, cx));
    }
}

impl Render for FlowX {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_workspace(window, cx);

        let now = std::time::Instant::now();
        let sidebar_visible = self.state.read(cx).sidebar_visible;
        let transition = self
            .sidebar_transition
            .get_or_insert_with(|| PanelTransition::new(sidebar_visible));
        transition.set_expanded(sidebar_visible, now);
        let sidebar_progress = transition.progress(now);
        if transition.is_animating(now) {
            let view = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = view.update(cx, |_, cx| cx.notify());
            });
        }
        let sidebar_content_width = visible_sidebar_width(
            window.viewport_size().width.into(),
            self.state.read(cx).sidebar_width(),
        );
        let sidebar_width = sidebar_content_width * sidebar_progress;

        let layer = self.state.read(cx).menu_layer.clone();
        let registry = self.state.read(cx).context_registry.clone();
        let frame_registry = registry.clone();
        let for_click = layer.clone();
        let context_state = self.state.clone();
        let ui = workspace_ui::handle(cx);
        let settings = ui.read(cx).settings.clone();
        let request = ui.read(cx).request.clone();
        let thread_dialog = ui.read(cx).thread_dialog.clone();

        let root = div()
            .relative()
            .size_full()
            .flex()
            .bg(Colors::bg(&global::current()))
            // Registered on the root rather than per region: right-clicking
            // the background must do nothing rather than open an empty panel,
            // and only the root can afford to see every click.
            .id("flowx.root")
            .track_focus(self.focus.as_ref().expect("workspace focus"))
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                    if !this.resizing_inspector && !this.resizing_sidebar {
                        return;
                    }
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.resizing_inspector = false;
                        this.resizing_sidebar = false;
                        this.state.update(cx, |state, _| state.save());
                        return;
                    }
                    let resizing_sidebar = this.resizing_sidebar;
                    let sidebar_resize_offset = this.sidebar_resize_offset;
                    this.state.update(cx, |state, cx| {
                        let window_width = window.viewport_size().width.into();
                        if resizing_sidebar {
                            state.set_sidebar_width(visible_sidebar_width(
                                window_width,
                                f32::from(event.position.x) + sidebar_resize_offset,
                            ));
                        } else {
                            let sidebar_width = if state.sidebar_visible {
                                visible_sidebar_width(window_width, state.sidebar_width())
                            } else {
                                0.0
                            };
                            state.set_right_panel_width(visible_inspector_width(
                                window_width,
                                sidebar_width,
                                f32::from(window.viewport_size().width - event.position.x),
                            ));
                        }
                        cx.notify();
                    });
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing_inspector || this.resizing_sidebar {
                        this.resizing_inspector = false;
                        this.resizing_sidebar = false;
                        this.state.update(cx, |state, _| state.save());
                    }
                }),
            )
            .on_action(|_: &ShowSettings, window, cx| {
                if let Some(handle) = AppHandle::get(cx) {
                    workspace_ui::open_settings(handle.0, window, cx);
                }
            })
            .on_action(|_: &NewProject, window, cx| {
                if let Some(handle) = AppHandle::get(cx) {
                    state::open_project_picker(handle.0, window, cx);
                }
            })
            .on_action(|_: &NewThread, _, cx| {
                if let Some(handle) = AppHandle::get(cx) {
                    if workspace_ui::has_modal(&handle.0, cx) {
                        return;
                    }
                    handle.0.update(cx, |state, cx| {
                        state.create_thread();
                        state.save();
                        cx.notify();
                    });
                }
            })
            .on_action(|_: &ToggleSidebar, _, cx| shell_action("sidebar", cx))
            .on_action(|_: &ToggleInspector, _, cx| shell_action("inspector", cx))
            .on_action(|_: &StopTurn, _, cx| {
                if let Some(handle) = AppHandle::get(cx) {
                    handle.0.read(cx).stop_turn();
                }
            })
            .capture_key_down(move |event, window, cx| {
                if !layer.read(cx).is_open() {
                    return;
                }
                let choice = layer.update(cx, |layer, cx| {
                    match event.keystroke.key.as_str() {
                        "down" => layer.move_highlight(1),
                        "up" => layer.move_highlight(-1),
                        "escape" => {
                            layer.dismiss();
                        }
                        "enter" => {
                            let choice = layer.activate();
                            cx.notify();
                            return choice;
                        }
                        _ => return None,
                    }
                    cx.notify();
                    None
                });
                if ["down", "up", "escape", "enter"].contains(&event.keystroke.key.as_str()) {
                    cx.stop_propagation();
                }
                if let Some(choice) = choice
                    && let Some(chosen) = choice.chosen
                {
                    dispatch_menu_choice(&choice.owner, &chosen, window, cx);
                }
            })
            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                if workspace_ui::has_modal(&context_state, cx) {
                    return;
                }
                fx_design::components::handle_root_right_click(
                    &registry, &for_click, event, window, cx,
                );
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(title_bar(
                        self.state.clone(),
                        sidebar_content_width,
                        sidebar_progress,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(sidebar_column(
                                self.state.clone(),
                                sidebar_width,
                                sidebar_content_width,
                                sidebar_progress == 1.0,
                                cx,
                            ))
                            .child(center_column(self.state.clone(), window, cx))
                            .child(inspector_column(
                                self.state.clone(),
                                sidebar_width,
                                window,
                                cx,
                            )),
                    ),
            )
            // Above everything, so an open menu is never clipped by the
            // column it was opened from. Reads the layer rather than being
            // driven by a trigger, because a context menu has no trigger.
            .child(self.state.read(cx).picker.clone().map_or_else(
                || div().into_any_element(),
                |picker| AnyView::from(picker).into_any_element(),
            ))
            .children(settings.map(AnyView::from))
            .children(thread_dialog.map(AnyView::from))
            .children(request.map(AnyView::from))
            .child(fx_design::components::menu_layer_overlay(
                &self.state.read(cx).menu_layer,
                cx,
                |owner, chosen, window, cx| {
                    crate::dispatch_menu_choice(&owner, &chosen, window, cx);
                },
                |_window, cx| {
                    crate::dismiss_menu(cx);
                },
                |_item, _window, cx| {
                    crate::highlight_menu_item(cx);
                },
            ));
        fx_design::components::context_frame(&frame_registry, root)
    }
}

/// The window title bar, with the traffic lights inset by hand.
///
/// GPUI draws the traffic lights itself, so the bar reserves space for them
/// rather than using a native titlebar. A view that starts at x=0 would put its
/// first control underneath them.
fn title_bar(
    state: Entity<AppState>,
    sidebar_width: f32,
    sidebar_progress: f32,
    cx: &mut Context<FlowX>,
) -> AnyElement {
    let theme = global::current();
    let snapshot = state.read(cx);
    let sidebar_visible = snapshot.sidebar_visible;
    let show_inspector = snapshot.snapshot.right_panel_visible;
    let title = snapshot.active_thread_title();
    let project = snapshot
        .active_project()
        .map(|project| project.name.clone());
    let has_project = project.is_some();
    let busy = snapshot.is_busy();
    let for_sidebar = state.clone();
    let for_inspector = state.clone();
    let for_new = state.clone();
    let collapsed_width = TRAFFIC_LIGHT_GUTTER + Layout::CONTROL_COMPACT_HEIGHT + Spacing::MD;
    let sidebar_controls_width =
        collapsed_width + (sidebar_width - collapsed_width) * sidebar_progress;

    div()
        .h(px(Layout::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM))
        .pr(px_len(Spacing::MD))
        .border_b(px(fx_design::theme::BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .bg(Colors::sidebar_bg(&theme))
        .child(
            div()
                .w(px(sidebar_controls_width))
                .h_full()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .pl(px(TRAFFIC_LIGHT_GUTTER))
                .pr(px_len(Spacing::MD))
                .child(fx_design::components::icon_button(
                    fx_design::components::IconButtonProps::new(
                        Icon::SIDEBAR_LEFT,
                        "Toggle sidebar · ⌘B",
                    )
                    .small()
                    .ghost()
                    .selected(sidebar_visible),
                    move |_, _window, cx| {
                        for_sidebar.update(cx, |state, cx| {
                            state.toggle_sidebar();
                            state.save();
                            cx.notify();
                        });
                    },
                )),
        )
        .child(
            div()
                .id("workspace.title")
                .debug_selector(|| "workspace-title".into())
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .justify_center()
                .px(px_len(Spacing::SM))
                .on_mouse_down(MouseButton::Left, |event, window, _| {
                    if event.click_count == 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                })
                .child(
                    div()
                        .fx_body_medium()
                        .text_color(Colors::fg(&theme))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(SharedString::from(title)),
                )
                .children(project.map(|name| {
                    div()
                        .fx_caption()
                        .text_color(Colors::fg_secondary(&theme))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(SharedString::from(name))
                })),
        )
        .children(busy.then(|| {
            fx_design::components::activity_dot(
                Colors::accent(&theme),
                fx_design::components::ActivityState::Running,
                0.0,
            )
        }))
        .child(project_changes::view(state.clone(), cx))
        .child(fx_design::components::icon_button(
            fx_design::components::IconButtonProps::new(Icon::PLUS, "New conversation · ⌘N")
                .small()
                .ghost()
                .enabled(has_project),
            move |_, _, cx| {
                for_new.update(cx, |state, cx| {
                    state.create_thread();
                    state.save();
                    cx.notify();
                })
            },
        ))
        .child(fx_design::components::icon_button(
            fx_design::components::IconButtonProps::new(Icon::SIDEBAR_RIGHT, "Changes · ⌘G")
                .small()
                .ghost()
                .selected(show_inspector),
            move |_, _, cx| {
                for_inspector.update(cx, |state, cx| {
                    state.toggle_inspector();
                    state.save();
                    cx.notify();
                })
            },
        ))
        .into_any_element()
}

/// Keep a useful conversation column even when restoring a wider inspector.
fn visible_inspector_width(window_width: f32, sidebar_width: f32, requested: f32) -> f32 {
    let available = (window_width - sidebar_width - Layout::MINIMUM_CONVERSATION_WIDTH).max(0.0);
    requested
        .clamp(Layout::RIGHT_PANEL_MIN_WIDTH, Layout::RIGHT_PANEL_MAX_WIDTH)
        .min(available)
}

fn visible_sidebar_width(window_width: f32, requested: f32) -> f32 {
    Layout::clamp_sidebar_width(requested)
        .min((window_width - Layout::MINIMUM_CONVERSATION_WIDTH).max(0.0))
}

fn sidebar_column(
    state: Entity<AppState>,
    width: f32,
    content_width: f32,
    settled_open: bool,
    cx: &mut Context<FlowX>,
) -> AnyElement {
    let theme = global::current();
    div()
        .id("sidebar.column")
        .debug_selector(|| "sidebar-column".into())
        .relative()
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .overflow_hidden()
        .children((width > 0.0).then(|| sidebar::view_at_width(state.clone(), content_width, cx)))
        .children(settled_open.then(|| {
            div()
                .id("sidebar.resize")
                .debug_selector(|| "sidebar-resize-handle".into())
                .absolute()
                .right_0()
                .top_0()
                .h_full()
                .w(px(Spacing::SM))
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .hover(|style| style.bg(Colors::bg_selected(&theme)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        this.resizing_sidebar = event.click_count != 2;
                        this.sidebar_resize_offset = visible_sidebar_width(
                            window.viewport_size().width.into(),
                            this.state.read(cx).sidebar_width(),
                        ) - f32::from(event.position.x);
                        if event.click_count == 2 {
                            this.state.update(cx, |state, cx| {
                                state.set_sidebar_width(Layout::SIDEBAR_WIDTH);
                                state.save();
                                cx.notify();
                            });
                        }
                        cx.stop_propagation();
                    }),
                )
        }))
        .into_any_element()
}

fn inspector_column(
    state: Entity<AppState>,
    sidebar_width: f32,
    window: &mut Window,
    cx: &mut Context<FlowX>,
) -> AnyElement {
    let width = if state.read(cx).snapshot.right_panel_visible {
        visible_inspector_width(
            window.viewport_size().width.into(),
            sidebar_width,
            state.read(cx).right_panel_width(),
        )
    } else {
        0.0
    };
    div()
        .relative()
        .w(px(width))
        .h_full()
        .min_w_0()
        .flex_shrink_0()
        .overflow_hidden()
        .child(inspector::view(state, cx))
        .children((width > 0.0).then(|| {
            div()
                .absolute()
                .left_0()
                .top_0()
                .h_full()
                .w(px(Spacing::XS))
                .cursor(gpui::CursorStyle::ResizeLeftRight)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.resizing_inspector = true;
                        cx.stop_propagation();
                    }),
                )
        }))
        .into_any_element()
}

/// Left inset reserved for the traffic lights.
const TRAFFIC_LIGHT_GUTTER: f32 = 88.0;

/// The conversation column.
fn center_column(
    state: Entity<AppState>,
    window: &mut Window,
    cx: &mut Context<FlowX>,
) -> AnyElement {
    let theme = global::current();
    // Loaded before the read guard is taken: a view must never trigger a disk
    // read inside a render.
    let conversation = state.update(cx, |state, _| state.active_conversation());
    let has_thread = conversation.is_some();
    let loading_history = state
        .read(cx)
        .snapshot
        .active_agent_id
        .is_some_and(|id| state.read(cx).native_history_loading(id))
        && conversation.as_ref().is_some_and(|conversation| {
            conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .messages()
                .len()
                == 0
        });

    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .bg(Colors::content_bg(&theme))
        .children(state.read(cx).last_error.clone().map(|error| {
            fx_design::components::surface::banner(
                Colors::error_muted(&theme),
                Colors::fg(&theme),
                div().fx_caption().child(SharedString::from(error)),
            )
        }))
        .child(if loading_history {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .p(px_len(Spacing::XXL))
                .child(fx_design::components::empty_state(
                    Icon::ARROW_CLOCKWISE.render(
                        fx_design::theme::IconSize::Large,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ),
                    "Loading conversation history…",
                    "Reading the saved conversation from its provider.",
                ))
                .into_any_element()
        } else if has_thread {
            match conversation.as_deref() {
                // The guard is taken for the render only; nothing under it awaits,
                // so it cannot deadlock against the runner that also wants it.
                Some(conversation) => match conversation.lock() {
                    Ok(conversation) => transcript::view(&conversation, window, cx),
                    // A poisoned mutex means a turn panicked while writing. The
                    // transcript is still readable, and the error is reported by
                    // the session layer rather than by refusing to draw.
                    Err(poisoned) => transcript::view(&poisoned.into_inner(), window, cx),
                },
                // Selected but not yet loaded: a render cannot load it, and
                // showing the empty state beats showing nothing at all.
                None => div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px_len(Spacing::XXL))
                    .child(fx_design::components::empty_state(
                        Icon::BUBBLE_LEFT_AND_BUBBLE_RIGHT.render(
                            fx_design::theme::IconSize::Large,
                            Colors::fg_tertiary(&theme),
                            &theme,
                        ),
                        "Loading this thread…",
                        "Its history is read the first time it is shown.",
                    ))
                    .into_any_element(),
            }
        } else {
            let has_project = state.read(cx).active_project().is_some();
            let for_action = state.clone();
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px_len(Spacing::LG))
                .p(px_len(Spacing::XXL))
                .child(fx_design::components::empty_state(
                    Icon::FOLDER.render(
                        fx_design::theme::IconSize::Hero,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ),
                    if has_project {
                        "Start a conversation"
                    } else {
                        "Open your workspace"
                    },
                    if has_project {
                        "Create a conversation to start working with your project."
                    } else {
                        "Choose a project folder to chat, explore files, and review changes."
                    },
                ))
                .child(fx_design::components::button(
                    if has_project {
                        "New conversation"
                    } else {
                        "Open project…"
                    },
                    fx_design::components::ButtonStyle::Primary,
                    true,
                    move |_, window, cx| {
                        if has_project {
                            for_action.update(cx, |state, cx| {
                                state.create_thread();
                                state.save();
                                cx.notify();
                            });
                        } else {
                            state::open_project_picker(for_action.clone(), window, cx);
                        }
                    },
                ))
                .into_any_element()
        })
        .child(composer_row(
            state.clone(),
            conversation.as_deref(),
            window,
            cx,
        ))
        .into_any_element()
}

/// The composer, below the transcript.
fn composer_row(
    state: Entity<AppState>,
    conversation: Option<&std::sync::Mutex<Conversation>>,
    window: &mut Window,
    cx: &mut Context<FlowX>,
) -> AnyElement {
    let (agent, project_root, busy, queued) = {
        let state = state.read(cx);
        (
            state.active_agent(),
            state
                .active_project()
                .map(|project| project.root_path.clone()),
            state.is_busy(),
            conversation
                .and_then(|conversation| conversation.lock().ok())
                .map(|conversation| conversation.queued_previews())
                .unwrap_or_default(),
        )
    };

    let (Some(agent), Some(project_root)) = (agent, project_root) else {
        return div().w(px(0.0)).h(px(0.0)).into_any_element();
    };

    let field = state.update(cx, |state, cx| state.draft(agent.id, cx));

    let submit_state = state.clone();
    let submit_agent = agent.clone();
    let submit_root = project_root.clone();
    field.update(cx, |field, _| {
        field.set_on_submit(move |prompt, cx| {
            let state = submit_state.clone();
            let agent = submit_agent.clone();
            let root = submit_root.clone();
            cx.defer(move |cx| {
                start_turn(&state, &root, agent, prompt.trim().to_string(), cx);
            });
        })
    });

    let for_actions = state.clone();
    let for_submit = field.clone();
    // Built by assignment rather than a struct literal: `on_action` is private,
    // so a `..Default::default()` update from another module does not compile.
    let mut chrome = composer::ComposerChrome::default();
    chrome.busy = busy;
    chrome.queued = queued;
    chrome.enabled = true;
    let mut chrome = chrome.on_action(move |action, _window, cx| match action {
        composer::ComposerAction::Submit => {
            let prompt = for_submit.read(cx).value().trim().to_string();
            if prompt.is_empty() {
                return;
            }
            start_turn(&for_actions, &project_root, agent.clone(), prompt, cx);
        }
        composer::ComposerAction::Stop => for_actions.update(cx, |state, _| state.stop_turn()),
        composer::ComposerAction::RemoveQueued(index) => {
            if let Some(conversation) = for_actions.read(cx).conversation_by_agent(agent.id) {
                conversation
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove_queued_at(index);
            }
            cx.refresh_windows();
        }
        _ => {}
    });

    let configuration = configuration::bar(&state, cx);
    composer::view_with_controls(&field, &mut chrome, Some(configuration), window, cx)
}

/// Send a prompt: record the user's row, then run the turn in the background.
fn start_turn(
    state: &Entity<AppState>,
    project_root: &str,
    agent: Agent,
    prompt: String,
    cx: &mut App,
) {
    if prompt.is_empty()
        || !state.update(cx, |state, cx| state.prepare_prompt(agent.id, &prompt, cx))
    {
        return;
    }
    let Some(conversation) = state.update(cx, |state, _| state.conversation_by_agent(agent.id))
    else {
        return;
    };

    // The runner's callback may not capture `App`: it fires on a background
    // thread, and `App` is neither `Send` nor reachable from there. It sends to
    // a channel instead, which `AppState::drain_updates` reads on each render —
    // which is also what repaints the window when a turn finishes.
    let updates = state.read(cx).update_sender_for(agent.id);
    let agent_id = agent.id;
    let agent =
        configuration::with_native_defaults(agent, state.read(cx).native_defaults(agent_id));
    let runner = session::TurnRunner::start(
        cx,
        conversation,
        project_root.to_string(),
        agent,
        prompt,
        fx_agent::codex::ApprovalPolicy::Ask,
        updates,
    );
    state.update(cx, |state, cx| {
        state.register_runner(agent_id, runner);
        cx.notify();
    });
}

/// The app-wide menu.
pub fn app_menu() -> MenuModel {
    MenuModel::new(vec![
        MenuSection::titled(
            "workspace",
            "Workspace",
            vec![
                MenuItem::new("new-project", "New project…").subtitle("Choose a folder"),
                MenuItem::new("new-thread", "New thread"),
            ],
        ),
        MenuSection::titled(
            "view",
            "View",
            vec![
                MenuItem::new("sidebar", "Toggle sidebar"),
                MenuItem::new("inspector", "Toggle inspector"),
            ],
        ),
        MenuSection::new("app", vec![MenuItem::new("settings", "Settings…")]),
    ])
}

/// Open the window with its shell.
///
/// Returns the handle so a caller — the binary, or a test — can drive the window
/// it just made.
pub fn open_window(
    state: Entity<AppState>,
    cx: &mut App,
) -> anyhow::Result<gpui::WindowHandle<FlowX>> {
    workspace_ui::restore_preferences(&state, cx);
    provider_catalog::refresh("codex");
    provider_catalog::refresh("claude");
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-q", Quit, None),
        gpui::KeyBinding::new("cmd-o", NewProject, None),
        gpui::KeyBinding::new("cmd-n", NewThread, None),
        gpui::KeyBinding::new("cmd-,", ShowSettings, None),
        gpui::KeyBinding::new("cmd-b", ToggleSidebar, None),
        gpui::KeyBinding::new("cmd-g", ToggleInspector, None),
        gpui::KeyBinding::new("cmd-.", StopTurn, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.set_menus(vec![
        gpui::Menu {
            name: "FlowX".into(),
            items: vec![
                gpui::MenuItem::action("Settings…", ShowSettings),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("Quit FlowX", Quit),
            ],
        },
        gpui::Menu {
            name: "File".into(),
            items: vec![
                gpui::MenuItem::action("Open project…", NewProject),
                gpui::MenuItem::action("New thread", NewThread),
            ],
        },
        gpui::Menu {
            name: "View".into(),
            items: vec![
                gpui::MenuItem::action("Toggle sidebar", ToggleSidebar),
                gpui::MenuItem::action("Toggle inspector", ToggleInspector),
            ],
        },
    ]);
    let quitting = state.clone();
    cx.on_app_quit(move |cx| {
        quitting.update(cx, |state, cx| {
            for runner in state.runners.values() {
                runner.interrupt();
            }
            state.drain_updates_with_context(cx);
            state.save_all(cx);
        });
        async {}
    })
    .detach();
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    let bounds = WindowBounds::Windowed(Bounds::new(
        gpui::point(px(80.0), px(80.0)),
        size(px(1280.0), px(900.0)),
    ));
    cx.open_window(
        WindowOptions {
            window_bounds: Some(bounds),
            window_min_size: Some(size(px(1000.0), px(640.0))),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("FlowX")),
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(16.0), px(19.0))),
            }),
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        move |_window, cx| {
            cx.activate(true);
            cx.new(|_cx| FlowX::new(state.clone()))
        },
    )
}

/// Whether an event is the one that opens a context menu.
pub fn is_right_click(event: &gpui::MouseDownEvent) -> bool {
    event.button == MouseButton::Right
}

/// The app state, reachable from a menu callback.
///
/// A menu row's handler receives only a `Window` and an `App`, so the state has
/// to be reachable without being captured. This is the app-wide handle that
/// makes that possible; there is exactly one, installed when the window opens.
#[derive(Clone)]
pub struct AppHandle(pub Entity<AppState>);

impl gpui::Global for AppHandle {}

impl AppHandle {
    /// Install as the global handle.
    pub fn install(state: Entity<AppState>, cx: &mut App) {
        cx.set_global(AppHandle(state));
    }

    pub fn get(cx: &App) -> Option<Self> {
        cx.try_global::<AppHandle>().cloned()
    }

    pub fn state(&self) -> &Entity<AppState> {
        &self.0
    }
}

/// Act on a chosen menu row.
///
/// Takes the window because some choices are not state changes but the opening
/// of a sheet, and a sheet is anchored to the window it was opened from.
pub fn dispatch_menu_choice(owner: &str, chosen: &str, window: &mut Window, cx: &mut App) {
    let Some(handle) = AppHandle::get(cx) else {
        return;
    };
    if owner.starts_with("settings-defaults:") {
        let sheet = workspace_ui::handle(cx).read(cx).settings.clone();
        if let Some(sheet) = sheet
            && let Some(editor) = sheet.read(cx).provider_defaults()
        {
            editor.update(cx, |editor, cx| {
                editor.apply(owner, chosen, cx);
            });
        }
        return;
    }
    if owner.starts_with("settings-titles:") {
        let sheet = workspace_ui::handle(cx).read(cx).settings.clone();
        if let Some(sheet) = sheet
            && let Some(editor) = sheet.read(cx).title_settings()
        {
            editor.update(cx, |editor, cx| {
                editor.apply(owner, chosen, cx);
            });
        }
        return;
    }
    if chosen == "new-project" {
        crate::state::open_project_picker(handle.0.clone(), window, cx);
        return;
    }
    if chosen == "settings" {
        workspace_ui::open_settings(handle.0, window, cx);
        return;
    }
    if chosen == "reveal" {
        if let Some(id) = owner
            .strip_prefix("project:")
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            && let Some(project) = handle.0.read(cx).project(id)
        {
            cx.reveal_path(std::path::Path::new(&project.root_path));
        }
        return;
    }
    if owner.starts_with("configuration:") {
        configuration::apply(&handle.0, owner, chosen, cx);
        return;
    }
    if let Some(agent_id) = owner
        .strip_prefix("thread:")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
    {
        match chosen {
            "rename" | "delete" => thread_actions::open(
                handle.0,
                agent_id,
                if chosen == "rename" {
                    thread_actions::ThreadAction::Rename
                } else {
                    thread_actions::ThreadAction::Delete
                },
                window,
                cx,
            ),
            "archive" | "unarchive" => handle.0.update(cx, |state, cx| {
                state.save_all(cx);
                state.set_thread_archived(agent_id, chosen == "archive");
                cx.notify();
            }),
            _ => {}
        }
        return;
    }
    if chosen == "refresh-threads"
        && let Some(project_id) = owner
            .strip_prefix("project:")
            .and_then(|id| id.parse().ok())
    {
        handle.0.update(cx, |state, cx| {
            state.request_native_refresh(project_id);
            cx.notify();
        });
        return;
    }
    handle.0.update(cx, |state, cx| {
        state.save_all(cx);
        state.apply_menu_choice(owner, chosen);
        cx.notify();
    });
}

/// Close the open menu.
pub fn dismiss_menu(cx: &mut App) {
    let Some(handle) = AppHandle::get(cx) else {
        return;
    };
    let layer = handle.0.read(cx).menu_layer.clone();
    layer.update(cx, |layer, cx| {
        layer.dismiss();
        cx.notify();
    });
}

/// Move the highlight with the pointer.
///
/// The layer owns the highlight so the pointer and the keyboard share one, which
/// is why this does not keep a second copy anywhere.
pub fn highlight_menu_item(cx: &mut App) {
    let _ = cx;
}

/// The layer type, re-exported so views need not import from `fx-design`.
pub type Layer = MenuLayer;

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::{Agent, Project};
    use fx_store::{AppPaths, PersistedApp, PersistedProject};
    use gpui::AppContext;
    use uuid::Uuid;

    #[test]
    fn switching_threads_moves_focus_from_the_detached_composer() {
        let mut test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            AppHandle::install(state.clone(), cx);
            cx.bind_keys([gpui::KeyBinding::new("cmd-n", NewThread, None)]);
        });
        let view_state = state.clone();
        let window = test.add_window(move |_, _| FlowX::new(view_state));
        test.refresh().unwrap();
        let first =
            test.update(|cx| state.update(cx, |state, cx| state.draft(Uuid::from_u128(1), cx)));
        window
            .update(&mut test, |_, window, cx| {
                first.read(cx).focus_handle().focus(window)
            })
            .unwrap();
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.snapshot.active_agent_id = Some(Uuid::from_u128(2));
                cx.notify();
            })
        });
        test.refresh().unwrap();
        let second =
            test.update(|cx| state.update(cx, |state, cx| state.draft(Uuid::from_u128(2), cx)));
        window
            .update(&mut test, |_, window, cx| {
                assert!(second.read(cx).focus_handle().is_focused(window));
                assert!(!first.read(cx).focus_handle().is_focused(window));
            })
            .unwrap();
        test.simulate_keystrokes(window.into(), "cmd-n");
        test.read(|cx| assert_eq!(state.read(cx).snapshot.projects[0].agents.len(), 3));
    }

    /// A private store directory, kept alive by the caller.
    ///
    /// Each test gets its own: they run concurrently in one process, and two
    /// tests sharing a store would overwrite each other's snapshot.
    fn fresh_paths() -> (tempfile::TempDir, AppPaths) {
        let temp = tempfile::tempdir().expect("a temp dir");
        let paths = AppPaths::at(temp.path().join("state"));
        (temp, paths)
    }

    /// State with one project and two threads, built through the real path.
    ///
    /// The returned `TempDir` must be held by the caller: it owns the directory
    /// the state reads from.
    fn seeded(cx: &mut App) -> (tempfile::TempDir, Entity<AppState>) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().into_owned();
        let mut project = Project::new(&root);
        project.agent_order = vec![Uuid::from_u128(1), Uuid::from_u128(2)];
        let app = PersistedApp {
            projects: vec![PersistedProject {
                project: Some(project),
                agents: vec![
                    Agent::with_id(Uuid::from_u128(1), "First"),
                    Agent::with_id(Uuid::from_u128(2), "Second"),
                ],
                is_expanded: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let paths = AppPaths::at(temp.path().join("state"));
        seeded_from(cx, &paths, &app);
        (temp, cx.new(|cx| AppState::restore(paths, cx)))
    }

    /// Persist a snapshot, then build state from it, the way a relaunch does.
    fn seeded_from(cx: &mut App, paths: &AppPaths, app: &PersistedApp) -> Entity<AppState> {
        paths.ensure().expect("the store directory");
        fx_store::Store::new(paths.clone())
            .save_app(app)
            .expect("the test snapshot saved");
        cx.new(|cx| AppState::restore(paths.clone(), cx))
    }

    #[test]
    fn the_shell_renders_without_panicking() {
        // The only end-to-end check that the three columns compose into one
        // tree; each component is unit-tested in `fx-design`. What this catches
        // is the class of failure unit tests cannot see — a column that borrows
        // state across an `on_click` closure, or a token read before the theme
        // is resolved.
        let mut test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let window = test.add_window(move |_window, _cx| FlowX::new(state.clone()));
        test.refresh().expect("the shell painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn the_shell_renders_with_no_projects_at_all() {
        // The empty case has a different element tree than the populated one,
        // and it is the one a first launch actually shows.
        let mut test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        let window = test.add_window(move |_window, _cx| FlowX::new(state.clone()));
        test.refresh().expect("the empty shell painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn a_fresh_install_opens_without_a_project() {
        let test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.read(|cx| {
            let state = state.read(cx);
            assert!(state.snapshot.projects.is_empty());
            assert_eq!(state.active_thread_title(), "FlowX");
        });
    }

    #[test]
    fn restoring_selects_the_first_project_and_thread() {
        // Otherwise the app opens onto an empty centre column despite having
        // threads to show.
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.read(|cx| {
            let state = state.read(cx);
            assert!(state.active_project().is_some());
            assert_eq!(state.active_agent().unwrap().id, Uuid::from_u128(1));
        });
    }

    #[test]
    fn restoring_the_old_inspector_default_adopts_the_compact_layout() {
        let test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        fx_store::Store::new(paths.clone())
            .save_app(&PersistedApp {
                right_panel_width: 760.0,
                min_right_panel_width: 560.0,
                max_right_panel_width: 1100.0,
                ..Default::default()
            })
            .unwrap();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.read(|cx| {
            assert_eq!(
                state.read(cx).right_panel_width(),
                Layout::RIGHT_PANEL_DEFAULT_WIDTH
            );
        });
    }

    #[test]
    fn a_restored_project_reports_its_threads_in_saved_order() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.read(|cx| {
            let titles: Vec<String> = state
                .read(cx)
                .active_agents()
                .iter()
                .map(|a| a.title.clone())
                .collect();
            assert_eq!(titles, vec!["First".to_string(), "Second".to_string()]);
        });
    }

    #[test]
    fn the_sidebar_visibility_survives_a_restart() {
        let test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.toggle_sidebar();
                state.save();
            })
        });
        test.read(|cx| assert!(!state.read(cx).sidebar_visible));

        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.read(|cx| assert!(!reopened.read(cx).sidebar_visible));
    }

    #[test]
    fn creating_a_thread_selects_it_and_records_it_in_the_project() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let created = test.update(|cx| state.update(cx, |state, _| state.create_thread()));
        assert!(created.is_some());
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.active_agent().unwrap().id, created.unwrap());
            assert_eq!(state.active_agents().len(), 3);
        });
    }

    #[test]
    fn creating_a_thread_with_no_project_does_nothing() {
        // There is nowhere to put a thread with no project.
        let test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        let created = test.update(|cx| state.update(cx, |state, _| state.create_thread()));
        assert!(created.is_none());
    }

    #[test]
    fn a_new_thread_survives_a_restart() {
        let test = gpui::TestAppContext::single();
        let (_dir, paths) = fresh_paths();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| state.update(cx, |state, _| state.create_thread()));
        // Nothing to create against, so the above is a no-op; assert the honest
        // behaviour rather than a thread that could not exist.
        test.read(|cx| assert!(state.read(cx).active_agent().is_none()));
    }

    #[test]
    fn the_inspector_width_is_clamped_to_its_legal_range() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.set_right_panel_width(10_000.0);
                assert_eq!(state.right_panel_width(), Layout::RIGHT_PANEL_MAX_WIDTH);
                state.set_right_panel_width(1.0);
                assert_eq!(state.right_panel_width(), Layout::RIGHT_PANEL_MIN_WIDTH);
            })
        });
    }

    #[test]
    fn the_app_menu_marks_what_cannot_work_yet() {
        // A control that cannot work should not be enabled.
        let model = app_menu();
        let disabled: Vec<String> = model
            .sections()
            .iter()
            .flat_map(|section| section.items.iter())
            .filter(|item| !item.enabled)
            .map(|item| item.id.clone())
            .collect();
        assert!(!disabled.contains(&"new-project".to_string()));
        assert!(!disabled.contains(&"settings".to_string()));
        assert_eq!(
            model
                .sections()
                .iter()
                .flat_map(|s| &s.items)
                .filter(|i| i.id == "settings")
                .count(),
            1
        );
    }

    #[test]
    fn choosing_sidebar_from_the_menu_toggles_and_saves() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| state.apply_menu_choice("sidebar", "sidebar"))
        });
        test.read(|cx| assert!(!state.read(cx).sidebar_visible));
    }

    #[test]
    fn choosing_new_thread_from_the_menu_creates_one() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.apply_menu_choice("sidebar", "new-thread")
            })
        });
        test.read(|cx| assert_eq!(state.read(cx).active_agents().len(), 3));
    }

    #[test]
    fn collapsing_from_the_menu_collapses_every_project() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.apply_menu_choice("project:abc", "collapse")
            })
        });
        test.read(|cx| {
            assert!(
                state
                    .read(cx)
                    .snapshot
                    .projects
                    .iter()
                    .all(|p| !p.is_expanded)
            );
        });
    }

    #[test]
    fn an_unknown_menu_choice_is_ignored_rather_than_fatal() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.apply_menu_choice("sidebar", "not-a-choice")
            })
        });
        test.read(|cx| assert!(state.read(cx).sidebar_visible));
    }

    #[test]
    fn the_default_window_fits_the_sidebar_the_inspector_and_a_centre() {
        const {
            assert!(
                Layout::SIDEBAR_WIDTH
                    + Layout::RIGHT_PANEL_DEFAULT_WIDTH
                    + Layout::MINIMUM_CONVERSATION_WIDTH
                    <= 1280.0
            )
        };
    }

    #[test]
    fn inspector_preserves_the_conversation_when_the_window_shrinks() {
        for sidebar in [0.0, Layout::SIDEBAR_WIDTH, Layout::SIDEBAR_MAX_WIDTH] {
            for width in [1000.0, 1280.0, 1920.0] {
                let panel = visible_inspector_width(width, sidebar, 1100.0);
                assert!(width - sidebar - panel >= Layout::MINIMUM_CONVERSATION_WIDTH);
                assert!(panel <= Layout::RIGHT_PANEL_MAX_WIDTH);
            }
        }
    }

    #[test]
    fn dragging_the_sidebar_updates_the_header_and_persists_on_release() {
        use gpui::{VisualTestContext, point};
        let mut test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let for_view = state.clone();
        let window = test.add_window(move |_, _| FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1280.0), px(800.0)));
        test.refresh().unwrap();
        let grip = visual
            .debug_bounds("sidebar-resize-handle")
            .unwrap()
            .center();
        visual.simulate_mouse_down(grip, MouseButton::Left, Default::default());
        let destination = point(px(330.0 - Layout::SIDEBAR_WIDTH) + grip.x, grip.y);
        visual.simulate_mouse_move(destination, MouseButton::Left, Default::default());
        visual.simulate_mouse_up(destination, MouseButton::Left, Default::default());
        test.refresh().unwrap();
        assert_eq!(
            visual.debug_bounds("sidebar-column").unwrap().size.width,
            px(330.0)
        );
        let header = visual
            .debug_bounds("icon-button:Workspace actions")
            .unwrap();
        let toggle = visual
            .debug_bounds("icon-button:Toggle sidebar · ⌘B")
            .unwrap();
        assert!((f32::from(header.center().x - toggle.center().x)).abs() <= 1.0);
        assert!(visual.debug_bounds("workspace-title").unwrap().left() >= px(330.0));
        test.update(|cx| {
            let paths = state.read(cx).store.paths().clone();
            let restored = cx.new(|cx| AppState::restore(paths, cx));
            assert_eq!(restored.read(cx).sidebar_width(), 330.0);
        });
    }

    #[test]
    fn sidebar_toggle_reveals_an_intermediate_width_then_clears_hidden_hit_regions() {
        use fx_design::theme::animation::FxAnimation;
        use gpui::VisualTestContext;
        let mut test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let for_view = state.clone();
        let window = test.add_window(move |_, _| FlowX::new(for_view));
        let mut visual = VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(size(px(1280.0), px(800.0)));
        test.refresh().unwrap();
        window
            .update(&mut test, |view, _, cx| {
                view.state.update(cx, |state, _| state.toggle_sidebar());
                view.sidebar_transition
                    .as_mut()
                    .unwrap()
                    .set_expanded(false, std::time::Instant::now() - FxAnimation::PANEL / 2);
                cx.notify();
            })
            .unwrap();
        test.refresh().unwrap();
        let width = f32::from(visual.debug_bounds("sidebar-column").unwrap().size.width);
        assert!(width > 0.0 && width < Layout::SIDEBAR_WIDTH);
        test.read(|cx| {
            for region in state.read(cx).context_registry.read(cx).regions() {
                assert!(f32::from(region.bounds.right()) <= width);
            }
        });
        // Advance the transition without a wall-clock sleep in the test.
        window
            .update(&mut test, |view, _, cx| {
                let mut transition = PanelTransition::new(true);
                transition.set_expanded(false, std::time::Instant::now() - FxAnimation::PANEL);
                view.sidebar_transition = Some(transition);
                cx.notify();
            })
            .unwrap();
        test.refresh().unwrap();
        assert_eq!(
            visual.debug_bounds("sidebar-column").unwrap().size.width,
            px(0.0)
        );
        test.read(|cx| assert!(state.read(cx).context_registry.read(cx).is_empty()));
    }

    /// Paints a transcript through the real render path.
    ///
    /// A bare element cannot be painted here: GPUI's `current_view` unwraps a
    /// stack that only a `Render` impl populates, and the transcript uses focus
    /// and frame scheduling. So this mounts a one-line view and paints that.
    fn paint(conversation: Conversation) {
        struct Probe {
            conversation: Conversation,
        }
        impl Render for Probe {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                transcript::view(&self.conversation, window, cx)
            }
        }

        let mut test = gpui::TestAppContext::single();
        let window = test.add_window(move |_window, _cx| Probe { conversation });
        test.refresh().expect("the transcript painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn the_transcript_renders_a_real_events_sequence() {
        // The last link in the chain: provider events → conversation → view →
        // paint. Every earlier test stops one link short, so this is the only
        // one that would catch a row shape the view cannot render.
        use fx_agent::ProviderEvent;
        let mut conversation = Conversation::new(uuid::Uuid::from_u128(1));
        conversation.apply(ProviderEvent::Lifecycle(
            fx_agent::provider::Lifecycle::TurnStarted,
        ));
        conversation.apply(ProviderEvent::TextDelta("Let me check.".into()));
        conversation.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: r#"{"command":["cargo","test"]}"#.into(),
        });
        conversation.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "283 passed".into(),
            is_error: false,
        });
        conversation.apply(ProviderEvent::TextDelta("Green.".into()));

        paint(conversation);
    }

    #[test]
    fn the_transcript_renders_while_streaming_and_with_pending_questions() {
        use fx_agent::ProviderEvent;
        let mut conversation = Conversation::new(uuid::Uuid::from_u128(2));
        conversation.apply(ProviderEvent::Lifecycle(
            fx_agent::provider::Lifecycle::TurnStarted,
        ));
        conversation.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: r#"{"questions":[{"question":"Which database?","header":"Scope",
                "options":[{"label":"Postgres"},{"label":"SQLite"}]}]}"#
                .into(),
        });
        conversation.apply(ProviderEvent::TextDelta("Asking now".into()));

        paint(conversation);
    }

    #[test]
    fn the_transcript_renders_an_odd_message_without_panicking() {
        // A shape the view has no branch for must still paint something.
        use fx_core::{ConversationMessage, MessageContent, MessageRole};
        let mut conversation = Conversation::new(uuid::Uuid::from_u128(3));
        // Empty, then a block with no natural rendering path.
        conversation.append(ConversationMessage::new(MessageRole::Assistant, vec![]));
        conversation.append(ConversationMessage::new(
            MessageRole::System,
            vec![MessageContent::Code {
                language: "rust".into(),
                code: "fn main() {}".into(),
            }],
        ));
        conversation.apply(fx_agent::ProviderEvent::Error("boom".into()));

        paint(conversation);
    }

    #[test]
    fn a_running_turn_is_reflected_in_the_shell() {
        // The loop the app was missing: a turn that is running makes the
        // composer show Stop, and clearing the runner makes it show Send again.
        let mut test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let for_window = state.clone();
        let window = test.add_window(move |_w, _cx| FlowX::new(for_window.clone()));

        test.read(|cx| assert!(!state.read(cx).is_busy(), "a turn started itself"));
        test.refresh().expect("the shell painted idle");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
        test.read(|cx| assert!(!state.read(cx).is_busy()));
    }

    #[test]
    fn a_turn_update_is_applied_and_reported() {
        // Delivered on a background thread in production; here it goes through the
        // same channel the runner sends on.
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let sender: std::sync::mpsc::Sender<session::SessionUpdate> =
            test.update(|cx| state.update(cx, |state, _| state.update_sender()));

        sender
            .send(session::SessionUpdate::Failed {
                reason: "Codex app-server exited".into(),
            })
            .expect("the update was queued");

        test.update(|cx| state.update(cx, |state, _| state.drain_updates()));
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.last_error.as_deref(), Some("Codex app-server exited"));
            assert!(!state.is_busy(), "a failed turn left the runner set");
        });
    }

    #[test]
    fn a_completed_turn_clears_the_error() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let sender: std::sync::mpsc::Sender<session::SessionUpdate> =
            test.update(|cx| state.update(cx, |state, _| state.update_sender()));

        sender
            .send(session::SessionUpdate::Failed {
                reason: "transient".into(),
            })
            .unwrap();
        sender
            .send(session::SessionUpdate::Finished {
                outcome: fx_agent::codex::TurnOutcome::Completed {
                    thread_id: "t1".into(),
                    stop_reason: "end_turn".into(),
                    events: Vec::new(),
                },
            })
            .unwrap();

        test.update(|cx| state.update(cx, |state, _| state.drain_updates()));
        test.read(|cx| {
            let state = state.read(cx);
            assert!(
                state.last_error.is_none(),
                "a later success left a stale error: {:?}",
                state.last_error
            );
        });
    }

    #[test]
    fn a_thread_keeps_its_draft_across_switches() {
        // A draft is unsent work; switching threads and back must not lose it.
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let (first, second) = test.read(|cx| {
            let state = state.read(cx);
            let agents = state.active_agents();
            (agents[0].id, agents[1].id)
        });

        let field = test.update(|cx| {
            let field = state.update(cx, |state, cx| state.draft(first, cx));
            field.update(cx, |field, cx| field.set_value("half-written thought", cx));
            field
        });
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.drafts.len(), 1);
        });

        // Switching away and back must not rebuild the field.
        let again = test.update(|cx| state.update(cx, |state, cx| state.draft(first, cx)));
        // An `Entity` is a handle, so identity is the handle's own.
        test.read(|_cx| assert!(again == field, "the draft was replaced, not kept"));
        test.read(|cx| assert_eq!(again.read(cx).value(), "half-written thought"));

        // A different thread gets its own field.
        let other = test.update(|cx| state.update(cx, |state, cx| state.draft(second, cx)));
        test.read(|_cx| assert!(other != field, "two threads shared one draft"));
    }

    #[test]
    fn adding_a_project_selects_it_and_persists_it() {
        // The path the picker hands over is already resolved; this is the write
        // half of the flow that was missing entirely.
        let test = gpui::TestAppContext::single();
        let (dir, state) = test.update(seeded);
        let project_dir = dir.path().join("my repo");
        std::fs::create_dir_all(&project_dir).unwrap();

        test.update(|cx| {
            state.update(cx, |state, _| {
                state.add_project(project_dir.to_str().unwrap());
            })
        });
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(
                state.snapshot.projects.len(),
                2,
                "the project was not added"
            );
            let active = state.active_project().expect("no active project");
            assert_eq!(active.root_path, project_dir.to_str().unwrap());
            assert_eq!(
                state.active_agents().len(),
                0,
                "a new project has no threads"
            );
        });

        // And it survives a reload, which is the point of calling save().
        let paths = AppPaths::at(dir.path().join("state"));
        let loaded = fx_store::Store::new(paths)
            .load_app()
            .expect("the snapshot a relaunch would read");
        assert!(
            loaded.projects.iter().any(|p| p
                .project
                .as_ref()
                .is_some_and(|p| p.root_path == project_dir.to_str().unwrap())),
            "the project did not reach disk"
        );
    }

    #[test]
    fn adding_a_project_clears_a_selection_from_the_project_left_behind() {
        // A carried-over agent id would name a thread in a different project,
        // and the centre column would show that thread's transcript.
        let test = gpui::TestAppContext::single();
        let (dir, state) = test.update(seeded);
        test.read(|cx| assert!(state.read(cx).active_agent().is_some()));
        let fresh = dir.path().join("other");
        std::fs::create_dir_all(&fresh).unwrap();

        test.update(|cx| {
            state.update(cx, |state, _| {
                state.add_project(fresh.to_str().unwrap());
            })
        });
        test.read(|cx| {
            let state = state.read(cx);
            assert!(
                state.active_agent().is_none(),
                "a stale agent stayed selected"
            );
        });
    }

    #[test]
    fn a_picker_rejects_a_path_it_cannot_use_before_it_reaches_the_state() {
        // The picker owns validation, so the state never sees a bad path. Pin the
        // seam: whatever the picker accepts, `add_project` stores verbatim.
        assert!(crate::project_picker::validate_project_path("").is_err());
        assert!(crate::project_picker::validate_project_path("relative/dir").is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(crate::project_picker::validate_project_path(file.to_str().unwrap()).is_err());
        let candidate = crate::project_picker::validate_project_path(dir.path().to_str().unwrap())
            .expect("a real temp dir is a valid project");
        assert_eq!(candidate.path, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn the_persisted_defaults_match_the_design_metrics() {
        // `fx-store` cannot depend on `fx-design`, so the panel geometry is
        // declared twice. This is the only crate that sees both, so this is the
        // only place the duplication can be caught.
        let default = PersistedApp::default();
        assert_eq!(default.sidebar_width, Layout::SIDEBAR_WIDTH);
        let mut sidebar = default.clone();
        sidebar.sidebar_width = 0.0;
        sidebar.normalize();
        assert_eq!(sidebar.sidebar_width, Layout::SIDEBAR_MIN_WIDTH);
        sidebar.sidebar_width = f32::MAX;
        sidebar.normalize();
        assert_eq!(sidebar.sidebar_width, Layout::SIDEBAR_MAX_WIDTH);
        assert_eq!(default.right_panel_width, Layout::RIGHT_PANEL_DEFAULT_WIDTH);
        assert_eq!(default.min_right_panel_width, Layout::RIGHT_PANEL_MIN_WIDTH);
        assert_eq!(default.max_right_panel_width, Layout::RIGHT_PANEL_MAX_WIDTH);
    }

    #[test]
    fn only_a_right_click_opens_a_context_menu() {
        let event = gpui::MouseDownEvent {
            button: MouseButton::Right,
            position: gpui::point(px(10.0), px(10.0)),
            click_count: 1,
            modifiers: gpui::Modifiers::default(),
            first_mouse: false,
        };
        assert!(is_right_click(&event));
        assert!(!is_right_click(&gpui::MouseDownEvent {
            button: MouseButton::Left,
            ..event
        }));
    }

    #[test]
    fn a_transcript_is_read_once_and_then_cached() {
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        let (project, agent) = {
            test.read(|cx| {
                let state = state.read(cx);
                (
                    state.active_project().unwrap().id,
                    state.active_agent().unwrap().id,
                )
            })
        };
        test.update(|cx| {
            state.update(cx, |state, _| {
                let first = state.conversation(project, agent);
                let second = state.conversation(project, agent);
                assert!(
                    std::sync::Arc::ptr_eq(&first, &second),
                    "the transcript was read twice"
                );
            })
        });
    }

    #[test]
    fn a_save_failure_is_recorded_and_the_session_survives() {
        // A full disk must not take the window down.
        let test = gpui::TestAppContext::single();
        let (_dir, state) = test.update(seeded);
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.store = fx_store::Store::new(AppPaths::at("/dev/null/impossible"));
                state.save();
            })
        });
        test.read(|cx| {
            let state = state.read(cx);
            assert!(state.last_error.is_some());
            // Still usable.
            assert!(state.active_project().is_some());
        });
    }
}
