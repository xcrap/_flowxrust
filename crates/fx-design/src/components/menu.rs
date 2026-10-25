//! Rendering for menus.
//!
//! Positioning and window clamping come from GPUI's [`anchored`] element, so
//! this file only draws the panel and handles dismissal.
//!
//! Two behaviours are carried over from the original design system deliberately:
//!
//! - The panel is sized to its content rather than carrying a shadow. A styled
//!   element shadow is clipped to the element's own bounds, so a menu that
//!   grows past its shadow looks wrong. Here the elevation is expressed with an
//!   edge and a surface fill instead, which cannot clip.
//! - A menu shows at most one open at a time. Rather than a broadcast, opening
//!   any menu closes the others through a shared [`MenuLayer`].

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, CursorStyle, Entity, InteractiveElement, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, StatefulInteractiveElement, StyleRefinement,
    Window, anchored, div, px,
};

use crate::components::menu_model::{MenuItem, MenuItemTone, MenuModel};
use crate::icon::Icon;
use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{Colors, Layout, Radii, Spacing, TextRole, global};

/// The one open menu, app-wide.
///
/// FlowX allows at most one dropdown or context menu at a time, so this holds
/// at most one. It is deliberately pure state with no callbacks: opening and
/// closing return what happened, and the caller — which has a `Window` and an
/// `App` — decides what to do about it.
///
/// That keeps the layer testable without a window and keeps it from holding a
/// closure that outlives the view that owned the trigger.
#[derive(Default)]
pub struct MenuLayer {
    open: Option<OpenMenu>,
    /// Monotonic counter, so successive opens get distinct element ids.
    generation: u64,
}

struct OpenMenu {
    model: MenuModel,
    owner: SharedString,
    generation: u64,
    position: Option<Point<Pixels>>,
}

/// What happened when a menu closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dismissed {
    /// Which trigger owned the menu.
    pub owner: SharedString,
    /// The activated row's id, or `None` when it closed without a choice.
    pub chosen: Option<String>,
}

impl MenuLayer {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn model(&self) -> Option<&MenuModel> {
        self.open.as_ref().map(|o| &o.model)
    }

    pub fn model_mut(&mut self) -> Option<&mut MenuModel> {
        self.open.as_mut().map(|o| &mut o.model)
    }

    /// Which trigger owns the open menu, if any.
    pub fn owner(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.owner.as_ref())
    }

    /// Whether `owner` is the one currently open.
    pub fn is_owner(&self, owner: &str) -> bool {
        self.owner() == Some(owner)
    }

    /// Element id for the open menu's panel, unique per open.
    pub fn generation(&self) -> u64 {
        self.open.as_ref().map(|o| o.generation).unwrap_or(0)
    }

    /// Anchor in window coordinates, when opened from a pointer event.
    pub fn position(&self) -> Option<Point<Pixels>> {
        self.open.as_ref().and_then(|open| open.position)
    }

    /// Open a menu, replacing whatever was open.
    pub fn open(&mut self, model: MenuModel, owner: impl Into<SharedString>) {
        self.open_positioned(model, owner.into(), None);
    }

    /// Open at the trigger or context click's location in the window.
    pub fn open_at(
        &mut self,
        model: MenuModel,
        owner: impl Into<SharedString>,
        position: Point<Pixels>,
    ) {
        self.open_positioned(model, owner.into(), Some(position));
    }

    fn open_positioned(
        &mut self,
        model: MenuModel,
        owner: SharedString,
        position: Option<Point<Pixels>>,
    ) {
        self.generation += 1;
        self.open = Some(OpenMenu {
            model,
            owner,
            generation: self.generation,
            position,
        });
    }

    /// Close without a choice.
    pub fn dismiss(&mut self) -> Option<Dismissed> {
        self.open.take().map(|open| Dismissed {
            owner: open.owner,
            chosen: None,
        })
    }

    /// Activate the highlighted row and close. A disabled or missing highlight
    /// closes the menu without choosing anything.
    pub fn activate(&mut self) -> Option<Dismissed> {
        let chosen = self.open.as_mut().and_then(|open| open.model.activate());
        self.open.take().map(|open| Dismissed {
            owner: open.owner,
            chosen,
        })
    }

    /// Toggle: close if this owner is open, otherwise open.
    pub fn toggle(
        &mut self,
        model: MenuModel,
        owner: impl Into<SharedString>,
    ) -> Option<Dismissed> {
        let owner = owner.into();
        if self.is_owner(&owner) {
            self.dismiss()
        } else {
            self.open(model, owner);
            None
        }
    }

    /// Apply a keyboard step to the highlight.
    pub fn move_highlight(&mut self, delta: isize) {
        if let Some(open) = self.open.as_mut() {
            open.model.move_highlight(delta);
        }
    }

    pub fn highlight_first(&mut self) {
        if let Some(open) = self.open.as_mut() {
            open.model.highlight_first();
        }
    }

    pub fn highlight_last(&mut self) {
        if let Some(open) = self.open.as_mut() {
            open.model.highlight_last();
        }
    }

    /// Highlight a row under the pointer, so keyboard navigation continues
    /// from wherever the pointer last was.
    pub fn highlight(&mut self, key: &str) {
        if let Some(open) = self.open.as_mut()
            && open.model.highlighted() != Some(key)
        {
            open.model.set_highlight(key);
        }
    }

    /// Highlight a row by section and item.
    pub fn highlight_item(&mut self, section_id: &str, item_id: &str) {
        if let Some(open) = self.open.as_mut() {
            open.model.highlight(section_id, item_id);
        }
    }
}

/// How a row resolves its colors, given the theme.
///
/// Split out so the states can be asserted without rendering.
#[derive(Debug, Clone, Copy)]
pub struct RowPalette {
    pub title: gpui::Rgba,
    pub subtitle: gpui::Rgba,
    pub background: gpui::Rgba,
    pub check: gpui::Rgba,
}

pub fn row_palette(item: &MenuItem, hovered: bool, theme: &crate::theme::Theme) -> RowPalette {
    use gpui::Rgba;
    let clear = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    let title = if !item.enabled {
        Colors::fg_tertiary(theme)
    } else if item.tone == MenuItemTone::Destructive {
        Colors::error(theme)
    } else {
        Colors::fg(theme)
    };

    RowPalette {
        title,
        // Subtitles are supporting information, so they stay one step down
        // even when the title is full-strength.
        subtitle: if item.enabled {
            Colors::fg_tertiary(theme)
        } else {
            Colors::fg_quaternary(theme)
        },
        background: if hovered && item.enabled {
            Colors::bg_hover(theme)
        } else {
            clear
        },
        check: menu_check_ink(theme),
    }
}

/// The 3:1 non-text floor a checkmark has to clear: it is the only thing that
/// says which choice is current.
const CHECK_CONTRAST_FLOOR: f64 = 3.0;

/// The selected row's checkmark: the accent where it reads on the menu, and
/// the strongest foreground where it does not.
///
/// The checklist's rule. On `bg_surface` the dark violet accent is 2.15:1,
/// and several light accents land between 2.1 and 3.0. Measured under the
/// hover wash too, because the pointer changes the row and not the mark.
fn menu_check_ink(theme: &crate::theme::Theme) -> gpui::Rgba {
    let surface = Colors::bg_surface(theme);
    crate::theme::legible_or(
        Colors::accent(theme),
        Colors::fg(theme),
        &[
            surface,
            crate::theme::composite(Colors::bg_hover(theme), surface),
        ],
        CHECK_CONTRAST_FLOOR,
    )
}

/// Render one row.
fn row(
    section_id: &str,
    item: &MenuItem,
    hovered: bool,
    show_selection: bool,
    on_choose: impl Fn(String, String, &mut Window, &mut App) + 'static,
    on_hover: impl Fn(String, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let theme = global::current();
    let palette = row_palette(item, hovered, &theme);
    let key = MenuModel::key_for(section_id, &item.id);
    let section_id = section_id.to_string();
    let item_id = item.id.clone();
    let enabled = item.enabled;
    let hover_key = key.clone();
    let press_key = key.clone();
    let on_hover = Rc::new(on_hover);
    let on_press = on_hover.clone();

    let mut content = div()
        .debug_selector(|| "menu-row".into())
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::XXS))
        .min_h(px_len(Layout::MENU_ROW_HEIGHT))
        .flex_shrink_0()
        .py(px_len(Spacing::XXS))
        .px(px_len(Spacing::SM))
        .bg(palette.background)
        .rounded(px_len(Radii::SM));

    // Choice menus align every title beside the selected item's checkmark.
    // Action menus have no selection and need no empty leading column.
    if show_selection {
        content = content.child(
            div()
                .w(px_len(Layout::MENU_CHECK_GUTTER))
                .flex()
                .flex_shrink_0()
                .flex_col()
                .items_center()
                .justify_center()
                .child(if item.selected {
                    Icon::CHECKMARK.render(crate::theme::IconSize::Small, palette.check, &theme)
                } else {
                    div().size(px_len(0.0)).into_any_element()
                }),
        );
    }

    let mut labels = div()
        .flex()
        .flex_1()
        .min_w_0()
        .flex_col()
        .items_start()
        .gap(px_len(Spacing::XXXS));
    labels = labels.child(
        div()
            .w_full()
            .debug_selector(|| "menu-title".into())
            .fx_role(TextRole::CaptionMedium)
            .text_color(palette.title)
            .truncate()
            .child(item.title.clone()),
    );
    if let Some(subtitle) = &item.subtitle {
        labels = labels.child(
            div()
                .w_full()
                .debug_selector(|| "menu-subtitle".into())
                .fx_role(TextRole::Caption)
                .text_color(palette.subtitle)
                .truncate()
                .child(subtitle.clone()),
        );
    }
    content = content.child(labels);

    content
        // `.id` upgrades to `Stateful<Div>`, which is what carries `on_click`
        // in GPUI 0.2.2. A row's id is its highlight key, so hover state
        // survives a rebuild of the menu.
        .id(SharedString::from(key))
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .hover(move |s: StyleRefinement| {
            if enabled {
                s.fx_background(Colors::bg_hover(&theme))
            } else {
                s
            }
        })
        .on_mouse_move(move |_, window, cx| {
            if enabled {
                on_hover(hover_key.clone(), window, cx);
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            move |_event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if enabled {
                    on_press(press_key.clone(), window, cx);
                    on_choose(section_id.clone(), item_id.clone(), window, cx);
                }
            },
        )
        .into_any_element()
}

/// Render the panel: sections, headers, rows, and the dividers between them.
pub fn menu_surface(
    model: &MenuModel,
    on_choose: impl Fn(String, String, &mut Window, &mut App) + 'static,
    on_mouse_move: impl Fn(&str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let on_choose = Rc::new(on_choose);
    let on_mouse_move = Rc::new(on_mouse_move);
    let theme = global::current();
    let show_selection = model
        .sections()
        .iter()
        .flat_map(|section| &section.items)
        .any(|item| item.selected);

    let mut body = div()
        .flex()
        .flex_col()
        .gap(px_len(0.0))
        .p(px_len(Spacing::XXS));

    for (index, section) in model.sections().iter().enumerate() {
        // Separate consecutive groups so a long menu stays readable.
        if index > 0 {
            body = body.child(
                div()
                    .h(px_len(1.0))
                    .my(px_len(Spacing::XXS))
                    .bg(Colors::border(&theme)),
            );
        }

        if let Some(title) = &section.title {
            body = body.child(
                div()
                    .px(px_len(Spacing::SM))
                    .pt(px_len(Spacing::SM))
                    .pb(px_len(Spacing::XXS))
                    .fx_role(TextRole::Overline)
                    .text_color(Colors::fg_tertiary(&theme))
                    .child(title.clone()),
            );
        }

        for item in &section.items {
            let key = MenuModel::key_for(&section.id, &item.id);
            let hovered = model.highlighted() == Some(key.as_str());
            let choose = Rc::clone(&on_choose);
            let hover = Rc::clone(&on_mouse_move);
            body = body.child(row(
                &section.id,
                item,
                hovered,
                show_selection,
                move |s, i, w, c| choose(s, i, w, c),
                move |k, w, c| hover(k.as_ref(), w, c),
            ));
        }
    }

    // The panel is sized to its content and given an edge rather than a shadow:
    // a styled shadow would be clipped to the panel's own bounds.
    div()
        .id("menu-panel")
        .flex()
        .flex_col()
        .w(px_len(Layout::MENU_WIDTH))
        .max_h(px_len(Layout::MENU_MAX_PANEL_HEIGHT))
        .overflow_y_scroll()
        .bg(Colors::bg_surface(&theme))
        .rounded(px_len(Radii::MD))
        .border_1()
        .border_color(Colors::border_medium(&theme))
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(body)
        .into_any_element()
}

/// Render the whole popup: an invisible backdrop that dismisses on click, plus
/// the anchored panel on top of it.
///
/// The backdrop is what makes "click anywhere else to close" work without any
/// global mouse or event monitor.
pub fn menu_overlay(
    model: &MenuModel,
    on_choose: impl Fn(String, String, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    on_mouse_move: impl Fn(&str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    menu_overlay_at(model, None, on_choose, on_dismiss, on_mouse_move)
}

fn menu_overlay_at(
    model: &MenuModel,
    position: Option<Point<Pixels>>,
    on_choose: impl Fn(String, String, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    on_mouse_move: impl Fn(&str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let backdrop = div()
        .absolute()
        .size_full()
        .id("menu-backdrop")
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            move |_event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                on_dismiss(window, cx);
            },
        );

    let mut panel = anchored()
        .offset(gpui::point(px(0.0), px(Layout::MENU_ANCHOR_GAP)))
        .snap_to_window_with_margin(gpui::Edges::all(px(Layout::MENU_EDGE_MARGIN)));
    if let Some(position) = position {
        panel = panel.position(position);
    }
    let panel = panel
        .child(menu_surface(model, on_choose, on_mouse_move))
        .into_any_element();

    div()
        .absolute()
        .size_full()
        .child(backdrop)
        .child(panel)
        .into_any_element()
}

/// A control that opens a menu when pressed.
pub struct MenuTrigger {
    model: MenuModel,
    owner: SharedString,
    label: SharedString,
    expanded: bool,
    enabled: bool,
    panel_width: Option<f32>,
}

impl MenuTrigger {
    pub fn new(
        model: MenuModel,
        owner: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            model,
            owner: owner.into(),
            label: label.into(),
            expanded: false,
            enabled: true,
            panel_width: None,
        }
    }

    pub fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn panel_width(mut self, width: f32) -> Self {
        self.panel_width = Some(width);
        self
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
}

/// Render a trigger, plus its menu when open.
///
/// The label renders differently when expanded, which is why it is a value
/// rather than a fixed string: a trigger needs to show that it is open.
/// Draw the open menu for a layer, if there is one.
///
/// The layer is app-wide but its *drawing* belongs to one place — the root of
/// the window — so that a panel opened from a context menu, a toolbar button, or
/// a keyboard shortcut all appear above the same content and none of them can be
/// clipped by the column they came from.
///
/// Renders nothing at all when the layer is closed, so a caller can include it
/// unconditionally.
pub fn menu_layer_overlay(
    layer: &Entity<MenuLayer>,
    cx: &App,
    on_choose: impl Fn(String, String, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    on_mouse_move: impl Fn(&str, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let Some(model) = layer.read(cx).model().cloned() else {
        return div().into_any_element();
    };
    let position = layer.read(cx).position();
    let generation = layer.read(cx).generation();
    let for_choice = layer.clone();
    let for_dismiss = layer.clone();
    let for_hover = layer.clone();
    menu_overlay_at(
        &model,
        position,
        move |section, item, window, cx| {
            let dismissed = for_choice.update(cx, |layer, cx| {
                if layer.generation() != generation {
                    return None;
                }
                layer.highlight_item(&section, &item);
                let dismissed = layer.activate();
                cx.notify();
                dismissed
            });
            if let Some(Dismissed {
                owner,
                chosen: Some(chosen),
            }) = dismissed
            {
                on_choose(owner.to_string(), chosen, window, cx);
            }
        },
        move |window, cx| {
            for_dismiss.update(cx, |layer, cx| {
                layer.dismiss();
                cx.notify();
            });
            on_dismiss(window, cx);
        },
        move |key, window, cx| {
            let changed = for_hover.update(cx, |layer, cx| {
                if layer.generation() != generation
                    || layer
                        .model()
                        .is_none_or(|model| model.highlighted() == Some(key))
                {
                    return false;
                }
                layer.highlight(key);
                cx.notify();
                true
            });
            if changed {
                on_mouse_move(key, window, cx);
            }
        },
    )
}

pub fn menu_trigger(trigger: MenuTrigger, layer: &Entity<MenuLayer>) -> AnyElement {
    let theme = global::current();
    let MenuTrigger {
        model,
        owner,
        label,
        expanded,
        enabled,
        panel_width: _,
    } = trigger;

    let label_color = if !enabled {
        Colors::fg_tertiary(&theme)
    } else if expanded {
        Colors::fg(&theme)
    } else {
        Colors::fg_secondary(&theme)
    };

    let content = div()
        .flex()
        .flex_row()
        .min_w_0()
        .max_w_full()
        .items_center()
        .gap(px_len(Spacing::XS))
        .px(px_len(Spacing::SM))
        .h(px_len(Layout::CONTROL_COMPACT_HEIGHT))
        .rounded(px_len(Radii::SM))
        .bg(if expanded {
            Colors::bg_selected(&theme)
        } else {
            gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }
        })
        .text_color(label_color)
        .fx_role(TextRole::CaptionMedium)
        .child(div().min_w_0().truncate().child(label.clone()))
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .child(Icon::CHEVRON_DOWN.render(
                    crate::theme::IconSize::Micro,
                    label_color,
                    &theme,
                )),
        );

    let mut element = content
        .id(SharedString::from(format!("menu-trigger:{owner}")))
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .hover(move |s: StyleRefinement| {
            if enabled {
                s.fx_background(Colors::bg_hover(&theme))
                    .fx_text_color(Colors::fg(&theme))
            } else {
                s
            }
        });

    if enabled {
        let layer_for_click = layer.clone();
        let model_for_click = model.clone();
        let owner_for_click = owner.clone();
        element = element.on_click(move |event: &ClickEvent, _window, cx| {
            cx.stop_propagation();
            layer_for_click.update(cx, |layer, cx| {
                if layer.is_owner(&owner_for_click) {
                    layer.dismiss();
                } else {
                    layer.open_at(
                        model_for_click.clone(),
                        owner_for_click.clone(),
                        event.position(),
                    );
                }
                cx.notify();
            });
        });
    }

    element.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::menu_model::MenuSection;
    use crate::theme::{AppearanceMode, BaseTone, Theme};

    fn themes() -> Vec<Theme> {
        let mut out = Vec::new();
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                out.push(Theme {
                    base_tone: tone,
                    appearance_mode: mode,
                    ..Default::default()
                });
            }
        }
        out
    }

    fn items() -> Vec<MenuItem> {
        vec![
            MenuItem::new("a", "Alpha").selected(true),
            MenuItem::new("b", "Beta").subtitle("with a note"),
            MenuItem::new("c", "Gamma").tone(MenuItemTone::Destructive),
            MenuItem::new("d", "Delta").enabled(false),
        ]
    }

    fn model() -> MenuModel {
        MenuModel::new(vec![MenuSection::titled("s", "Section", items())])
    }

    // ── The layer ──────────────────────────────────────────────────────────

    #[test]
    fn a_layer_starts_closed() {
        let layer = MenuLayer::default();
        assert!(!layer.is_open());
        assert!(layer.model().is_none());
        assert!(layer.owner().is_none());
        assert_eq!(layer.generation(), 0);
    }

    #[test]
    fn opening_records_the_owner_and_the_model() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "provider-picker");
        assert!(layer.is_open());
        assert_eq!(layer.owner(), Some("provider-picker"));
        assert!(layer.is_owner("provider-picker"));
        assert!(!layer.is_owner("model-picker"));
        assert_eq!(layer.model().unwrap().sections().len(), 1);
    }

    #[test]
    fn pointer_opening_preserves_the_anchor_until_the_menu_closes() {
        let mut layer = MenuLayer::default();
        let position = gpui::point(px(120.0), px(80.0));
        layer.open_at(model(), "project:example", position);
        assert_eq!(layer.position(), Some(position));
        layer.highlight_item("s", "b");
        let dismissed = layer.activate().unwrap();
        assert_eq!(dismissed.owner, "project:example");
        assert_eq!(dismissed.chosen.as_deref(), Some("b"));
        assert_eq!(layer.position(), None);

        layer.open_at(model(), "project:other", position);
        layer.open(model(), "keyboard-menu");
        assert_eq!(
            layer.position(),
            None,
            "new menus must not inherit a stale anchor"
        );
    }

    #[test]
    fn only_one_menu_is_open_at_a_time() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "first");
        layer.open(model(), "second");
        assert_eq!(
            layer.owner(),
            Some("second"),
            "the first menu was not closed"
        );
    }

    #[test]
    fn toggling_the_open_owner_closes_it() {
        let mut layer = MenuLayer::default();
        assert!(
            layer.toggle(model(), "a").is_none(),
            "opening reports nothing"
        );
        assert!(layer.is_open());

        let dismissed = layer
            .toggle(model(), "a")
            .expect("closing reports the owner");
        assert_eq!(dismissed.owner, "a");
        assert_eq!(dismissed.chosen, None);
        assert!(!layer.is_open());
    }

    #[test]
    fn toggling_a_different_owner_replaces_the_menu() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "first");
        assert!(layer.toggle(model(), "second").is_none());
        assert_eq!(layer.owner(), Some("second"));
    }

    #[test]
    fn activating_closes_the_menu_and_reports_the_choice() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "a");
        layer.highlight_item("s", "b");
        let dismissed = layer.activate().expect("activate should close");
        assert_eq!(dismissed.chosen.as_deref(), Some("b"));
        assert_eq!(dismissed.owner, "a");
        assert!(!layer.is_open());
    }

    #[test]
    fn activating_with_nothing_highlighted_closes_without_choosing() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "a");
        let dismissed = layer.activate().expect("activate should still close");
        assert_eq!(dismissed.chosen, None);
    }

    #[test]
    fn dismissing_a_closed_layer_reports_nothing() {
        let mut layer = MenuLayer::default();
        assert!(layer.dismiss().is_none());
        layer.open(model(), "a");
        assert!(layer.dismiss().is_some());
        assert!(layer.dismiss().is_none(), "dismissing twice is harmless");
    }

    #[test]
    fn pointer_and_keyboard_share_one_highlight() {
        // The design intent: pressing a row highlights it, so the next arrow
        // key continues from the pointer rather than from the top.
        let mut layer = MenuLayer::default();
        layer.open(model(), "a");
        layer.highlight_item("s", "b");
        assert_eq!(layer.model().unwrap().highlighted(), Some("s|b"));

        layer.move_highlight(1);
        assert_eq!(
            layer.model().unwrap().highlighted(),
            Some("s|c"),
            "arrow down continued from the pointer, and never reached the disabled row"
        );
    }

    #[test]
    fn highlighting_by_full_key_works_for_the_pointer() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "a");
        layer.highlight("s|c");
        assert_eq!(layer.model().unwrap().highlighted(), Some("s|c"));
        layer.highlight("s|missing");
        assert_eq!(layer.model().unwrap().highlighted(), None);
    }

    #[test]
    fn keyboard_helpers_are_inert_while_closed() {
        let mut layer = MenuLayer::default();
        layer.move_highlight(1);
        layer.highlight_first();
        layer.highlight_last();
        layer.highlight("anything");
        assert!(!layer.is_open());
    }

    #[test]
    fn each_open_gets_a_distinct_generation() {
        let mut layer = MenuLayer::default();
        layer.open(model(), "a");
        let first = layer.generation();
        layer.dismiss();
        layer.open(model(), "a");
        assert_ne!(first, layer.generation());
    }

    // ── Row colors ─────────────────────────────────────────────────────────

    #[test]
    fn a_disabled_row_recedes_on_both_text_weights() {
        for theme in themes() {
            let enabled = row_palette(&MenuItem::new("x", "X"), false, &theme);
            let disabled = row_palette(&MenuItem::new("x", "X").enabled(false), false, &theme);
            assert_ne!(enabled.title, disabled.title);
            assert_ne!(enabled.subtitle, disabled.subtitle);
        }
    }

    #[test]
    fn a_disabled_row_never_gets_a_hover_fill() {
        let theme = Theme::default();
        let disabled = MenuItem::new("x", "X").enabled(false);
        assert_eq!(row_palette(&disabled, true, &theme).background.a, 0.0);
    }

    #[test]
    fn a_hovered_enabled_row_gets_the_hover_fill() {
        let theme = Theme::default();
        let item = MenuItem::new("x", "X");
        assert_eq!(row_palette(&item, false, &theme).background.a, 0.0);
        assert!(row_palette(&item, true, &theme).background.a > 0.0);
    }

    #[test]
    fn a_destructive_row_uses_the_error_hue() {
        let theme = Theme::default();
        let p = row_palette(
            &MenuItem::new("x", "X").tone(MenuItemTone::Destructive),
            false,
            &theme,
        );
        assert_eq!(p.title, Colors::error(&theme));
    }

    #[test]
    fn a_subtitle_sits_below_its_title_and_stays_readable() {
        for theme in themes() {
            let p = row_palette(&MenuItem::new("x", "X"), false, &theme);
            let surface = Colors::bg_surface(&theme);
            let title = crate::theme::contrast(p.title, surface);
            let subtitle = crate::theme::contrast(p.subtitle, surface);
            assert!(
                subtitle <= title,
                "a subtitle at {subtitle}:1 is stronger than its title at {title}:1"
            );
            assert!(
                subtitle >= 3.0,
                "a subtitle at {subtitle}:1 in {:?} is too faint",
                theme.appearance_mode
            );
        }
    }

    #[test]
    fn a_disabled_row_title_stays_readable_enough_to_explain_itself() {
        // A disabled row exists to say *why* something is unavailable, so its
        // title still has to be legible.
        for theme in themes() {
            let p = row_palette(&MenuItem::new("x", "X").enabled(false), false, &theme);
            let ratio = crate::theme::contrast(p.title, Colors::bg_surface(&theme));
            assert!(
                ratio >= 3.0,
                "a disabled title at {ratio}:1 in {:?} cannot be read",
                theme.appearance_mode
            );
        }
    }

    #[test]
    fn a_checkmark_is_visible_on_the_menu_and_keeps_the_accent_where_it_can() {
        // The checkmark alone says which choice is current, so it needs the
        // 3:1 a meaningful graphic does, with the row hovered or not.
        let mut kept = 0;
        for theme in themes() {
            for accent in crate::theme::AccentColor::ALL {
                let theme = Theme {
                    accent_color: accent,
                    ..theme
                };
                let surface = Colors::bg_surface(&theme);
                let hovered = crate::theme::composite(Colors::bg_hover(&theme), surface);
                let fills = [surface, hovered];
                let accent_reads = fills
                    .iter()
                    .all(|fill| crate::theme::contrast(Colors::accent(&theme), *fill) >= 3.0);
                for row_hovered in [false, true] {
                    let check = row_palette(&items()[0], row_hovered, &theme).check;
                    for fill in fills {
                        let ratio = crate::theme::contrast(check, fill);
                        assert!(
                            ratio >= 3.0,
                            "{accent:?} {:?} {:?}: the checkmark is {ratio}:1",
                            theme.base_tone,
                            theme.appearance_mode
                        );
                    }
                    if accent_reads {
                        assert_eq!(check, Colors::accent(&theme), "{accent:?} lost its hue");
                    }
                }
                kept += usize::from(accent_reads);
            }
        }
        assert!(kept > 0, "no accent keeps its hue on a menu");
    }

    // ── Geometry ───────────────────────────────────────────────────────────
}
