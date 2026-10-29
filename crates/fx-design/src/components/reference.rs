//! Shared selection treatment for code references and search results.

use crate::styled::px_len;
use crate::theme::{BorderWidth, Colors, global};
use gpui::{Div, IntoElement, ParentElement, Styled, div};

/// An inset edge leaves the source's own diff colors readable beneath it.
pub fn reference_selection(content: impl IntoElement, selected: bool) -> Div {
    let theme = global::current();
    div()
        .border_l(px_len(BorderWidth::STRONG))
        .border_color(if selected {
            Colors::fg(&theme)
        } else {
            Colors::border_subtle(&theme)
        })
        .child(content)
}
