//! A compact, clickable summary of a working tree's changed lines.

use gpui::prelude::*;
use gpui::{AnyElement, App, ClickEvent, SharedString, Window, div};

use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{Colors, Layout, Radii, Spacing, TextRole, global};

pub fn change_indicator(
    additions: usize,
    deletions: usize,
    file_count: usize,
    selected: bool,
    detail: String,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let theme = global::current();
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
        .when(selected, |this| this.bg(Colors::bg_selected(&theme)))
        .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
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
                        .text_color(Colors::success(&theme))
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .debug_selector(|| "project-deleted-count".into())
                        .text_color(Colors::error(&theme))
                        .child(SharedString::from(format!("−{deletions}"))),
                )
        }))
        .children((additions == 0 && deletions == 0).then(|| {
            div()
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from(format!(
                    "{file_count} {}",
                    if file_count == 1 { "file" } else { "files" }
                )))
        }))
        .into_any_element()
}
