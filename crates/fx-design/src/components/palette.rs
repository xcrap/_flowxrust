//! The command palette's rows, and the key chip they share with Settings.
//!
//! A palette row is wider and richer than a menu row — an icon, a title over a
//! subtitle, and the command's shortcut — so it is its own component rather
//! than a stretched [`menu_surface`](super::menu_surface) row. What it shares
//! with menus is the decision half: the palette keeps its highlight in a
//! [`MenuModel`](super::MenuModel), and pointer and keyboard move the same one.

use gpui::prelude::*;
use gpui::{AnyElement, App, CursorStyle, MouseButton, SharedString, Window, div};

use crate::icon::Icon;
use crate::styled::{FxStyled, px_len};
use crate::theme::{
    BorderWidth, Colors, IconSize, Layout, Radii, Shadow, Spacing, TextRole, Theme, global,
};

/// A row's colours, split out so its states are asserted without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaletteRowPalette {
    pub background: gpui::Rgba,
    pub icon: gpui::Rgba,
    pub title: gpui::Rgba,
    pub subtitle: gpui::Rgba,
}

/// The colours of a row, highlighted or not.
///
/// The highlight is the selection fill rather than the hover fill: it marks
/// what Return will run, which is a stronger statement than where the pointer
/// happens to be.
pub fn palette_row_palette(highlighted: bool, theme: &Theme) -> PaletteRowPalette {
    PaletteRowPalette {
        background: if highlighted {
            Colors::bg_selected(theme)
        } else {
            gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }
        },
        icon: Colors::accent(theme),
        title: Colors::fg(theme),
        subtitle: Colors::fg_tertiary(theme),
    }
}

/// What a palette row shows.
pub struct PaletteRow {
    /// Stable across rebuilds, so the row keeps its identity as the list filters.
    pub id: SharedString,
    pub icon: Icon,
    pub title: SharedString,
    pub subtitle: SharedString,
    /// The command's keys, already written as symbols.
    pub shortcut: Option<SharedString>,
    pub highlighted: bool,
}

/// One palette row.
///
/// `on_hover` moves the shared highlight to this row, and `on_choose` runs it.
/// The press is taken on mouse-down, as menu rows take theirs, so the panel
/// closes before the release lands on whatever is underneath.
pub fn palette_row(
    row: PaletteRow,
    on_hover: impl Fn(&mut Window, &mut App) + 'static,
    on_choose: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let theme = global::current();
    let palette = palette_row_palette(row.highlighted, &theme);
    let selector = format!("palette-row:{}", row.title);

    let labels = div()
        .flex()
        .flex_1()
        .min_w_0()
        .flex_col()
        .gap(px_len(Spacing::XXXS))
        .child(
            div()
                .w_full()
                .fx_role(TextRole::BodyMedium)
                .text_color(palette.title)
                .truncate()
                .child(row.title),
        )
        .child(
            div()
                .w_full()
                .fx_role(TextRole::Caption)
                .text_color(palette.subtitle)
                .truncate()
                .child(row.subtitle),
        );

    div()
        .id(row.id)
        .debug_selector(move || selector.clone())
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::MD))
        .px(px_len(Spacing::LG))
        .py(px_len(Spacing::MD))
        .rounded(px_len(Radii::LG))
        .bg(palette.background)
        .cursor(CursorStyle::PointingHand)
        .on_mouse_move(move |_, window, cx| on_hover(window, cx))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_choose(window, cx);
        })
        .child(
            div()
                .w(px_len(Spacing::XL))
                .flex()
                .flex_shrink_0()
                .justify_center()
                .child(row.icon.render(IconSize::Control, palette.icon, &theme)),
        )
        .child(labels)
        .children(row.shortcut.map(shortcut_chip))
        .into_any_element()
}

/// Keys written as symbols, on a small chip: `⇧⌘Z`.
///
/// Monospaced so that a column of shortcuts lines up the way a menu's does.
/// The chip carries its own fill and ink, so it reads the same on a highlighted
/// palette row as on the Settings panel.
pub fn shortcut_chip(keys: impl Into<SharedString>) -> AnyElement {
    let theme = global::current();
    div()
        .flex_shrink_0()
        .px(px_len(Spacing::XS))
        .py(px_len(Spacing::XXXS))
        .rounded(px_len(Radii::XS))
        .bg(Colors::bg_surface(&theme))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .fx_role(TextRole::MonoSmall)
        .text_color(Colors::fg_secondary(&theme))
        .child(keys.into())
        .into_any_element()
}

/// The palette's floating panel: the elevated surface, a hairline edge, and the
/// palette shadow, around `content`.
///
/// Elevated rather than the surface fill [`panel`](super::panel) uses, as the
/// Swift palette was: a highlighted row's subtitle stays legible over it, where
/// over the lighter surface it would not.
pub fn palette_panel(content: impl IntoElement) -> AnyElement {
    let theme = global::current();
    div()
        .w(px_len(Layout::COMMAND_PALETTE_WIDTH))
        .max_w_full()
        // Room for the shadow to land: a styled element's shadow is clipped to
        // the element's own bounds.
        .p(px_len(Spacing::SM))
        .fx_shadow(Shadow::palette(Colors::overlay(&theme)))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(palette_surface(&theme))
                .rounded(px_len(Radii::XXL))
                .border(px_len(BorderWidth::HAIRLINE))
                .border_color(Colors::border_medium(&theme))
                .child(content),
        )
        .into_any_element()
}

/// The fill behind the palette's rows.
pub fn palette_surface(theme: &Theme) -> gpui::Rgba {
    Colors::bg_elevated(theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AccentColor, AppearanceMode, BaseTone};

    fn every_theme() -> Vec<Theme> {
        let mut themes = Vec::new();
        for tone in BaseTone::ALL {
            for appearance in [AppearanceMode::Light, AppearanceMode::Dark] {
                themes.push(Theme {
                    base_tone: tone,
                    appearance_mode: appearance,
                    accent_color: AccentColor::Blue,
                    ..Theme::default()
                });
            }
        }
        themes
    }

    #[test]
    fn only_the_highlighted_row_is_filled() {
        for theme in every_theme() {
            let idle = palette_row_palette(false, &theme);
            let highlighted = palette_row_palette(true, &theme);
            assert_eq!(idle.background.a, 0.0);
            assert_eq!(highlighted.background, Colors::bg_selected(&theme));
            // Nothing else changes, so the highlight is the only signal.
            assert_eq!(idle.title, highlighted.title);
            assert_eq!(idle.subtitle, highlighted.subtitle);
        }
    }

    #[test]
    fn a_row_and_its_chip_stay_legible_on_the_palette_surface() {
        use crate::theme::{composite, contrast};
        for theme in every_theme() {
            let palette = palette_row_palette(true, &theme);
            // The highlight is a fill laid over the palette's own surface.
            let surface = palette_surface(&theme);
            let highlighted = composite(palette.background, surface);
            assert!(contrast(palette.title, highlighted) >= 7.0, "{theme:?}");
            // Tertiary ink's own floor, as on any surface it is drawn on.
            assert!(contrast(palette.subtitle, highlighted) >= 3.0, "{theme:?}");
            assert!(contrast(palette.subtitle, surface) >= 4.5, "{theme:?}");
            // The chip's ink on its own fill, wherever the chip is put.
            let chip = Colors::bg_surface(&theme);
            assert!(
                contrast(Colors::fg_secondary(&theme), chip) >= 4.5,
                "{theme:?}"
            );
        }
    }
}
