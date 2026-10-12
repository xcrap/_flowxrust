//! Hover tooltips.
//!
//! FlowX leans on tooltips instead of permanent labels in dense toolbars, so this
//! has to look deliberate: a small elevated panel with a hairline edge, matching
//! the menu language rather than the system one.
//!
//! GPUI 0.2.2 builds a tooltip from an `AnyView`, so the body is a real view
//! rather than an element tree. That also means it gets its own layout and
//! paint, which is why it can be styled independently of its anchor.

use gpui::{
    AnyView, App, AppContext as _, Context, IntoElement, ParentElement as _, Render, SharedString,
    Styled, Window, div, px,
};

use crate::styled::{FxStyled, px_len};
use crate::theme::{Colors, Radii, Spacing, TextRole, global};

/// Gap between the anchor and its tooltip.
const TOOLTIP_GAP: f32 = 6.0;
/// Maximum width before the label wraps.
const TOOLTIP_MAX_WIDTH: f32 = 280.0;

/// A tooltip body.
pub struct Tooltip {
    text: SharedString,
}

impl Tooltip {
    pub fn new(text: impl Into<SharedString>) -> Self {
        Self { text: text.into() }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        div()
            // The margin is what separates the tooltip from the control it
            // describes; GPUI places the tooltip against the anchor's edge.
            .mt(px(TOOLTIP_GAP))
            .max_w(px(TOOLTIP_MAX_WIDTH))
            .px(px_len(Spacing::SM))
            .py(px_len(Spacing::XXS + 2.0))
            .rounded(px_len(Radii::SM))
            .bg(Colors::bg_surface(&theme))
            .border_1()
            .border_color(Colors::border_medium(&theme))
            .fx_role(TextRole::Caption)
            .text_color(Colors::fg_secondary(&theme))
            .child(self.text.clone())
    }
}

/// Build a tooltip for `text`.
pub fn tooltip(text: impl Into<SharedString>, _window: &mut Window, cx: &mut App) -> AnyView {
    AnyView::from(cx.new(|_cx| Tooltip::new(text)))
}
