//! The preferences vocabulary: sections of grouped rows, each a title, a line
//! of explanation and the control that changes it.
//!
//! A section is a heading over one group. A group is a card whose rows are
//! split by hairlines rather than gaps, so a run of related choices reads as
//! one object. The control sits at the row's trailing edge and is never
//! stretched; the text beside it wraps instead.

use gpui::prelude::*;
use gpui::{AnyElement, App, ClickEvent, CursorStyle, SharedString, StyleRefinement, Window, div};

use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, global};

/// Opacity of a switch that cannot be flipped, matching the buttons.
const DISABLED_OPACITY: f32 = 0.52;

/// How a group's card is painted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingsGroupPalette {
    pub fill: gpui::Rgba,
    pub edge: gpui::Rgba,
    /// The hairline between two rows.
    pub rule: gpui::Rgba,
}

/// The elevated fill the sidebar uses, over the content background: the same
/// one step of depth the shell already reads as "a panel".
pub fn settings_group_palette(theme: &crate::theme::Theme) -> SettingsGroupPalette {
    SettingsGroupPalette {
        fill: Colors::panel_bg(theme),
        edge: Colors::border(theme),
        rule: Colors::border_subtle(theme),
    }
}

/// A heading, an optional line under it, and the group it introduces.
pub fn settings_section(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    body: impl IntoElement,
) -> gpui::Div {
    settings_section_with_action(title, description, None, body)
}

/// A section whose heading carries an action at its trailing edge, such as
/// re-checking what the rows report.
pub fn settings_section_with_action(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    action: Option<AnyElement>,
    body: impl IntoElement,
) -> gpui::Div {
    let theme = global::current();
    let heading = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXS))
        .px(px_len(Spacing::XXS))
        .child(
            div()
                .fx_title3()
                .text_color(Colors::fg(&theme))
                .child(title.into()),
        )
        .children(description.map(|description| {
            div()
                .fx_caption()
                .text_color(Colors::fg_secondary(&theme))
                .child(description)
        }));
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::MD))
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_end()
                .gap(px_len(Spacing::MD))
                .child(div().flex_1().min_w_0().child(heading))
                .children(action.map(|action| div().flex_shrink_0().child(action))),
        )
        .child(body)
}

/// One card holding `rows`, with a hairline between each pair.
pub fn settings_group(rows: impl IntoIterator<Item = AnyElement>) -> gpui::Div {
    let theme = global::current();
    let palette = settings_group_palette(&theme);
    let mut group = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .bg(palette.fill)
        .rounded(px_len(Radii::LG))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(palette.edge);
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            group = group.child(
                div()
                    .flex_shrink_0()
                    .h(px_len(BorderWidth::HAIRLINE))
                    .w_full()
                    .bg(palette.rule),
            );
        }
        group = group.child(row);
    }
    group
}

/// A row's title and, under it, what the setting does.
pub fn settings_row_label(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
) -> gpui::Div {
    let theme = global::current();
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXXS))
        .child(
            div()
                .fx_body_medium()
                .text_color(Colors::fg(&theme))
                .child(title.into()),
        )
        .children(description.map(|description| {
            div()
                .fx_caption()
                .text_color(Colors::fg_secondary(&theme))
                .child(description)
        }))
}

/// The common row: a title, its explanation, and the control.
pub fn settings_row(
    title: impl Into<SharedString>,
    description: Option<SharedString>,
    control: impl IntoElement,
) -> gpui::Div {
    settings_row_custom(settings_row_label(title, description), control)
}

/// A row whose leading side is drawn by the caller.
pub fn settings_row_custom(label: impl IntoElement, control: impl IntoElement) -> gpui::Div {
    row_frame(label, control).py(px_len(Spacing::MD))
}

/// A one-line row, for long reference lists such as the shortcut table.
pub fn settings_row_dense(label: impl IntoElement, control: impl IntoElement) -> gpui::Div {
    row_frame(label, control).py(px_len(Spacing::SM))
}

fn row_frame(label: impl IntoElement, control: impl IntoElement) -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap(px_len(Spacing::LG))
        .px(px_len(Spacing::LG))
        .child(div().flex_1().min_w_0().child(label))
        .child(div().flex_shrink_0().child(control))
}

/// A quiet label over a run of navigation destinations.
pub fn settings_nav_label(label: impl Into<SharedString>) -> gpui::Div {
    let theme = global::current();
    div()
        .w_full()
        .px(px_len(Spacing::MD))
        .pt(px_len(Spacing::LG))
        .pb(px_len(Spacing::XXS))
        .fx_caption()
        .text_color(Colors::fg_tertiary(&theme))
        .child(label.into())
}

/// Where the reader is: every step but the last is secondary, separated by a
/// slash, and the last is the page itself.
pub fn breadcrumb(trail: &[SharedString]) -> gpui::Div {
    let theme = global::current();
    let mut row = div()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM));
    for (index, step) in trail.iter().enumerate() {
        if index > 0 {
            row = row.child(
                div()
                    .fx_body()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child("/"),
            );
        }
        let last = index + 1 == trail.len();
        row = row.child(
            div()
                .whitespace_nowrap()
                .fx_role(if last {
                    TextRole::BodyMedium
                } else {
                    TextRole::Body
                })
                .text_color(if last {
                    Colors::fg(&theme)
                } else {
                    Colors::fg_secondary(&theme)
                })
                .child(step.clone()),
        );
    }
    row
}

/// A search field in a navigation column: a glyph, the caller's field, and
/// the key that reaches it.
pub fn settings_search(field: impl IntoElement, shortcut: impl Into<SharedString>) -> gpui::Div {
    let theme = global::current();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px_len(Spacing::XS))
        .h(px_len(Layout::CONTROL_HEIGHT))
        .px(px_len(Spacing::SM))
        .rounded(px_len(Radii::SM))
        .bg(Colors::input_bg(&theme))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .child(crate::icon::Icon::MAGNIFYINGGLASS.render(
            IconSize::Control,
            Colors::fg_tertiary(&theme),
            &theme,
        ))
        .child(div().flex_1().min_w_0().child(field))
        .child(super::shortcut_chip(shortcut))
}

/// How a switch is painted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwitchPalette {
    pub track: gpui::Rgba,
    pub knob: gpui::Rgba,
}

/// On is the accent with its own ink for the knob, the pairing every solid
/// accent fill already guarantees. Off is a recessed track whose knob is the
/// secondary ink, so the knob's position stays readable without the hue.
pub fn switch_palette(on: bool, theme: &crate::theme::Theme) -> SwitchPalette {
    if on {
        SwitchPalette {
            track: Colors::accent(theme),
            knob: Colors::on_accent(theme),
        }
    } else {
        SwitchPalette {
            track: Colors::bg_pressed(theme),
            knob: Colors::fg_secondary(theme),
        }
    }
}

/// An on/off switch. The knob sits at the trailing end when on.
///
/// There is no transform to slide the knob with, so it jumps; the track's
/// color carries the change.
pub fn switch(
    id: impl Into<SharedString>,
    on: bool,
    enabled: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = global::current();
    let palette = switch_palette(on, &theme);
    let inset = Spacing::XXXS;
    let knob = Layout::SWITCH_HEIGHT - inset * 2.0;
    let id = id.into();
    let debug = format!("switch:{id}");
    div()
        .debug_selector(move || debug.clone())
        .flex()
        .flex_shrink_0()
        .items_center()
        .when(on, |track| track.justify_end())
        .w(px_len(Layout::SWITCH_WIDTH))
        .h(px_len(Layout::SWITCH_HEIGHT))
        .p(px_len(inset))
        .rounded(px_len(Layout::SWITCH_HEIGHT / 2.0))
        .bg(palette.track)
        .child(
            div()
                .size(px_len(knob))
                .rounded(px_len(knob / 2.0))
                .bg(palette.knob),
        )
        .opacity(if enabled { 1.0 } else { DISABLED_OPACITY })
        .id(id)
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
        })
        .active(move |s: StyleRefinement| {
            if enabled {
                s.fx_opacity(super::button::PRESSED_OPACITY)
            } else {
                s
            }
        })
        .on_click(move |event, window, cx| {
            if enabled {
                on_toggle(event, window, cx);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AccentColor, AppearanceMode, BaseTone, Theme, contrast};

    fn every_theme() -> impl Iterator<Item = Theme> {
        BaseTone::ALL.into_iter().flat_map(|tone| {
            [AppearanceMode::Light, AppearanceMode::Dark]
                .into_iter()
                .flat_map(move |mode| {
                    AccentColor::ALL.into_iter().map(move |accent| Theme {
                        base_tone: tone,
                        appearance_mode: mode,
                        accent_color: accent,
                        ..Default::default()
                    })
                })
        })
    }

    #[test]
    fn a_switch_knob_clears_non_text_contrast_on_its_track_either_way() {
        for theme in every_theme() {
            let group = settings_group_palette(&theme).fill;
            for on in [true, false] {
                let palette = switch_palette(on, &theme);
                let track = crate::theme::composite(palette.track, group);
                let ratio = contrast(palette.knob, track);
                assert!(
                    ratio >= 3.0,
                    "{:?} {:?} {:?} on={on}: knob {ratio:.2}:1",
                    theme.base_tone,
                    theme.appearance_mode,
                    theme.accent_color,
                );
            }
        }
    }

    #[test]
    fn on_and_off_never_share_a_track() {
        for theme in every_theme() {
            assert_ne!(
                switch_palette(true, &theme).track,
                switch_palette(false, &theme).track
            );
        }
    }

    #[test]
    fn row_text_clears_aa_on_a_group() {
        for theme in every_theme() {
            let fill = settings_group_palette(&theme).fill;
            for (name, ink) in [
                ("fg", Colors::fg(&theme)),
                ("fg_secondary", Colors::fg_secondary(&theme)),
            ] {
                let ratio = contrast(ink, fill);
                assert!(
                    ratio >= 4.5,
                    "{name} on a settings group in {:?} {:?}: {ratio:.2}:1",
                    theme.base_tone,
                    theme.appearance_mode,
                );
            }
        }
    }
}
