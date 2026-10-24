//! A labelled checkbox, for choosing several items from a list.

use gpui::prelude::*;
use gpui::{App, ClickEvent, CursorStyle, SharedString, StyleRefinement, Window, div};

use crate::icon::Icon;
use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{BorderWidth, Colors, IconSize, Radii, Spacing, TextRole, Typography, global};

/// Opacity of a checkbox that cannot be toggled, matching the buttons.
const DISABLED_OPACITY: f32 = 0.52;

/// How the box itself looks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckboxPalette {
    pub fill: gpui::Rgba,
    pub edge: gpui::Rgba,
    pub check: gpui::Rgba,
}

pub fn checkbox_palette(checked: bool, theme: &crate::theme::Theme) -> CheckboxPalette {
    if checked {
        CheckboxPalette {
            fill: Colors::accent(theme),
            edge: Colors::accent(theme),
            check: Colors::on_accent(theme),
        }
    } else {
        CheckboxPalette {
            fill: Colors::bg_surface(theme),
            edge: Colors::border_medium(theme),
            check: Colors::bg_surface(theme),
        }
    }
}

/// A box and its label; the whole row toggles.
pub fn checkbox(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    checked: bool,
    enabled: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = global::current();
    let palette = checkbox_palette(checked, &theme);
    let edge = Typography::icon(IconSize::Medium, &theme).size;
    let id = id.into();
    let debug = format!("checkbox:{id}");
    let mut mark = div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px_len(edge))
        .rounded(px_len(Radii::XS))
        .border(px_len(BorderWidth::REGULAR))
        .border_color(palette.edge)
        .bg(palette.fill);
    if checked {
        mark = mark.child(Icon::CHECKMARK.render(IconSize::Micro, palette.check, &theme));
    }
    div()
        .debug_selector(move || debug.clone())
        .flex()
        .min_w_0()
        .items_center()
        .gap(px_len(Spacing::XS))
        .child(mark)
        .child(
            div()
                .min_w_0()
                .truncate()
                .fx_role(TextRole::Caption)
                .text_color(Colors::fg_secondary(&theme))
                .child(label.into()),
        )
        .opacity(if enabled { 1.0 } else { DISABLED_OPACITY })
        .id(id)
        .hover(move |s: StyleRefinement| {
            if enabled {
                s.fx_text_color(Colors::fg(&theme))
            } else {
                s
            }
        })
        .cursor(if enabled {
            CursorStyle::PointingHand
        } else {
            CursorStyle::default()
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
    use crate::theme::{AccentColor, AppearanceMode, BaseTone, Theme};

    #[test]
    fn a_checked_box_is_legible_on_every_accent() {
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                for accent in AccentColor::ALL {
                    let theme = Theme {
                        base_tone: tone,
                        appearance_mode: mode,
                        accent_color: accent,
                        ..Default::default()
                    };
                    let on = checkbox_palette(true, &theme);
                    let off = checkbox_palette(false, &theme);
                    assert!(crate::theme::contrast(on.check, on.fill) >= 3.0);
                    assert_ne!(on.fill, off.fill, "checked and unchecked look alike");
                }
            }
        }
    }
}
