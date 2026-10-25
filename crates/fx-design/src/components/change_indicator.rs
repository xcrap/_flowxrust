//! A compact, clickable summary of a working tree's changed lines.

use gpui::prelude::*;
use gpui::{AnyElement, App, ClickEvent, Rgba, SharedString, Window, div};

use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{Colors, Layout, Radii, Spacing, TextRole, Theme, composite, global};

/// AA for text: the counts are 12pt mono.
const COUNT_CONTRAST_FLOOR: f64 = 4.5;

/// What the indicator paints, split out so it can be asserted without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChangeIndicatorPalette {
    pub added: Rgba,
    pub removed: Rgba,
    /// The "N files" label shown when no line counts are known.
    pub files: Rgba,
    pub background: Rgba,
}

/// The fill the indicator sits on: the title bar's.
pub fn change_indicator_surface(theme: &Theme) -> Rgba {
    Colors::sidebar_bg(theme)
}

/// The indicator's colors, selected (its panel is open) or not, with the
/// pointer over it or not.
///
/// The counts use the diff inks rather than `success` and `error`: light-mode
/// `success` was 3.4:1 on the title bar and 2.9:1 selected. Their color does
/// not change under the pointer, so each has to read on every fill its
/// selection state can show, and falls back to `fg` where it does not — dark
/// `diff_removed_fg` is 4.2:1 on the selected fill.
///
/// A selected indicator keeps its fill under the pointer. Replacing it with
/// the lighter hover wash read as the selection dropping away.
pub fn change_indicator_palette(
    selected: bool,
    hovered: bool,
    theme: &Theme,
) -> ChangeIndicatorPalette {
    let fill = |hovered: bool| {
        if selected {
            Colors::bg_selected(theme)
        } else if hovered {
            Colors::bg_hover(theme)
        } else {
            Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }
        }
    };
    let surface = change_indicator_surface(theme);
    let shown = [false, true].map(|hovered| composite(fill(hovered), surface));
    let ink = |hue| crate::theme::legible_or(hue, Colors::fg(theme), &shown, COUNT_CONTRAST_FLOOR);
    ChangeIndicatorPalette {
        added: ink(Colors::diff_added_fg(theme)),
        removed: ink(Colors::diff_removed_fg(theme)),
        files: ink(Colors::fg_secondary(theme)),
        background: fill(hovered),
    }
}

pub fn change_indicator(
    additions: usize,
    deletions: usize,
    file_count: usize,
    selected: bool,
    detail: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let theme = global::current();
    let palette = change_indicator_palette(selected, false, &theme);
    let hover = change_indicator_palette(selected, true, &theme).background;
    div()
        .id("project-change-indicator")
        .debug_selector(|| "project-change-indicator".into())
        .flex()
        .flex_shrink_0()
        .items_center()
        .h(px_len(Layout::CONTROL_COMPACT_HEIGHT))
        .gap(px_len(Spacing::SM))
        .px(px_len(Spacing::SM))
        .rounded(px_len(Radii::SM))
        .whitespace_nowrap()
        .fx_role(TextRole::MonoSmall)
        .cursor_pointer()
        .bg(palette.background)
        .hover(|style| style.fx_background(hover))
        .active(|style| {
            style
                .fx_background(Colors::bg_pressed(&theme))
                .opacity(0.94)
        })
        .on_click(on_click)
        .tooltip(move |window, cx| super::tooltip(detail.clone(), window, cx))
        .children((additions > 0 || deletions > 0).then(|| {
            div()
                .flex()
                .gap(px_len(Spacing::SM))
                .child(
                    div()
                        .debug_selector(|| "project-added-count".into())
                        .text_color(palette.added)
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .debug_selector(|| "project-deleted-count".into())
                        .text_color(palette.removed)
                        .child(SharedString::from(format!("−{deletions}"))),
                )
        }))
        .children((additions == 0 && deletions == 0).then(|| {
            div()
                .text_color(palette.files)
                .child(SharedString::from(format!(
                    "{file_count} {}",
                    if file_count == 1 { "file" } else { "files" }
                )))
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selected_indicator_keeps_its_fill_under_the_pointer() {
        let theme = Theme::default();
        let selected = change_indicator_palette(true, false, &theme);
        assert_eq!(selected.background, Colors::bg_selected(&theme));
        assert_eq!(
            change_indicator_palette(true, true, &theme).background,
            selected.background
        );
        assert_eq!(
            change_indicator_palette(false, true, &theme).background,
            Colors::bg_hover(&theme)
        );
        assert_eq!(
            change_indicator_palette(false, false, &theme).background.a,
            0.0
        );
    }
}
