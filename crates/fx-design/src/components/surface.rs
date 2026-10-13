//! Surfaces and separators: cards, dividers, and the flat panel look.

use gpui::{IntoElement, ParentElement, Styled, StyledText, div};

use super::rich_text::{InlineSpan, InlineStyle, inline_run};
use crate::styled::{FxStyled, px_len};
use crate::theme::{BorderWidth, Colors, Radii, Spacing, TextRole, global};

/// Which way a divider runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DividerAxis {
    #[default]
    Horizontal,
    Vertical,
}

/// A one-point rule in the border color.
pub fn divider_with_axis(axis: DividerAxis) -> impl IntoElement {
    let theme = global::current();
    let rule = div().flex_shrink_0().bg(Colors::border_subtle(&theme));
    match axis {
        DividerAxis::Horizontal => rule.h(px_len(BorderWidth::HAIRLINE)).w_full(),
        DividerAxis::Vertical => rule.w(px_len(BorderWidth::HAIRLINE)).h_full(),
    }
}

/// A horizontal divider, the common case.
pub fn divider() -> impl IntoElement {
    divider_with_axis(DividerAxis::Horizontal)
}

/// A padded surface: `bg_surface` fill, hairline edge, `lg` radius.
///
/// This is FlowX's default container. It is deliberately opaque — no material,
/// no vibrancy, no translucency — because a translucent card over an opaque
/// window is the single thing that most reliably makes a custom UI look like a
/// native one.
pub fn card(content: impl IntoElement, padding: f32) -> impl IntoElement {
    let theme = global::current();
    div()
        .p(px_len(padding))
        .bg(Colors::bg_surface(&theme))
        .rounded(px_len(Radii::LG))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border(&theme))
        .child(content)
}

/// A card at the default padding.
pub fn card_default(content: impl IntoElement) -> impl IntoElement {
    card(content, Spacing::MD)
}

/// The floating-panel look: `bg_surface` fill, `border_medium` edge, `xl`
/// radius. Used by menus and the command palette.
pub fn panel(content: impl IntoElement) -> impl IntoElement {
    let theme = global::current();
    div()
        .bg(Colors::bg_surface(&theme))
        .rounded(px_len(Radii::XL))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_medium(&theme))
        .child(content)
}

/// An empty-state block: a large muted glyph, a title, and a hint.
pub fn empty_state(
    icon: impl IntoElement,
    title: impl Into<gpui::SharedString>,
    hint: impl Into<gpui::SharedString>,
) -> impl IntoElement {
    let theme = global::current();
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .text_center()
        .gap(px_len(Spacing::SM))
        .py(px_len(Spacing::XXL))
        .px(px_len(Spacing::LG))
        .child(icon)
        .child(
            div()
                .fx_role(crate::theme::TextRole::BodyMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child(title.into()),
        )
        .child(
            div()
                .fx_role(crate::theme::TextRole::Caption)
                .text_color(Colors::fg_tertiary(&theme))
                .child(hint.into()),
        )
}

/// A banner: a tinted fill carrying same-hue text.
///
/// The `*_muted` tokens are deliberately translucent, so the surface behind the
/// banner shows through and the banner reads as a wash rather than a slab.
pub fn banner(tint: gpui::Rgba, ink: gpui::Rgba, content: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px_len(Spacing::SM))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .rounded(px_len(Radii::MD))
        .bg(tint)
        // The ink cascades to the banner's children, so a caller only has to
        // pass the hue once.
        .text_color(ink)
        .child(content)
}

/// A single-line notice used above a panel or inside a header.
pub fn inline_notice(ink: gpui::Rgba, content: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .text_color(ink)
        .child(content)
}

/// The hairline used under a section header inside a panel.
pub fn section_divider() -> impl IntoElement {
    divider()
}

/// A section header in the sidebar style, wrapping in narrow panels.
pub fn section_header(label: impl Into<gpui::SharedString>) -> impl IntoElement {
    let theme = global::current();
    div()
        .w_full()
        .min_w_0()
        .fx_overline()
        .text_color(Colors::fg_secondary(&theme))
        .child(label.into())
}

/// Window title and optional context share one baseline and truncate together.
pub fn window_title(title: &str, context: Option<&str>) -> impl IntoElement {
    let theme = global::current();
    let mut text = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title_end = text.len();
    let mut runs = vec![inline_run(
        &InlineSpan {
            range: 0..title_end,
            style: InlineStyle::default(),
        },
        TextRole::BodyMedium,
        &theme,
    )];
    if let Some(context) = context.filter(|context| !context.trim().is_empty()) {
        text.push_str(" · ");
        text.push_str(&context.split_whitespace().collect::<Vec<_>>().join(" "));
        let mut context_run = inline_run(
            &InlineSpan {
                range: title_end..text.len(),
                style: InlineStyle::default(),
            },
            TextRole::Body,
            &theme,
        );
        context_run.color = Colors::fg_secondary(&theme).into();
        runs.push(context_run);
    }
    div()
        .w_full()
        .min_w_0()
        .fx_body_medium()
        // GPUI 0.2.2 caches nowrap's unconstrained first measurement. A one-line
        // clamp remeasures at the final width so the ellipsis is actually drawn.
        .whitespace_normal()
        .text_ellipsis()
        .line_clamp(1)
        .child(StyledText::new(text).with_runs(runs))
}

/// The hairline thickness used for every card and panel edge.
pub const HAIRLINE: f32 = BorderWidth::HAIRLINE;
