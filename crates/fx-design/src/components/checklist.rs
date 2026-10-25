//! A read-only checklist: a task list as someone works through it.
//!
//! Each row is a mark and a line of text. Done rows recede, the active row
//! carries the accent and the motion, and rows not started stay neutral, so the
//! eye lands on the one step in progress without reading the list.

use gpui::prelude::*;
use gpui::{AnyElement, IntoElement, Rgba, SharedString, div, px};

use super::{spinner_arc, static_ring};
use crate::icon::Icon;
use crate::styled::{FxStyled, px_len};
use crate::theme::{BorderWidth, Colors, IconSize, Radii, Spacing, TextRole, Theme, Typography};

/// Where one row of a checklist is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecklistMark {
    Pending,
    /// The step being worked on.
    Active,
    Done,
}

impl ChecklistMark {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Active, Self::Done];
}

/// What one row paints, split out so it can be asserted without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChecklistInk {
    pub mark: Rgba,
    pub text: Rgba,
}

/// The 3:1 non-text floor a mark has to clear, since it is the only thing
/// that says where its step is.
const MARK_CONTRAST_FLOOR: f64 = 3.0;

/// The ink for each kind of row.
///
/// The active and done marks use their hue wherever it is legible on the
/// checklist's surface, and the strongest foreground where it is not — the
/// same rule as a badge's label. Light-mode emerald and orange accents fall
/// short of 3:1, and a spinner nobody can see says nothing.
pub fn checklist_ink(mark: ChecklistMark, theme: &Theme) -> ChecklistInk {
    let legible = |hue: Rgba| {
        if crate::theme::contrast(hue, checklist_surface(theme)) >= MARK_CONTRAST_FLOOR {
            hue
        } else {
            Colors::fg(theme)
        }
    };
    match mark {
        ChecklistMark::Pending => ChecklistInk {
            mark: Colors::fg_tertiary(theme),
            text: Colors::fg_secondary(theme),
        },
        ChecklistMark::Active => ChecklistInk {
            mark: legible(Colors::accent(theme)),
            text: Colors::fg(theme),
        },
        ChecklistMark::Done => ChecklistInk {
            mark: legible(Colors::success(theme)),
            text: Colors::fg_tertiary(theme),
        },
    }
}

/// The fill a checklist sits on, which every ink above is measured against.
pub fn checklist_surface(theme: &Theme) -> Rgba {
    Colors::bg_elevated(theme)
}

/// A checklist's container: a header line, then its rows.
pub fn checklist(header: impl IntoElement, rows: Vec<AnyElement>) -> impl IntoElement {
    let theme = crate::theme::global::current();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XS))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .rounded(px_len(Radii::LG))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .bg(checklist_surface(&theme))
        .child(header)
        .children(rows)
}

/// One row of a checklist.
///
/// `spin` turns the active row's mark into a spinner at that angle; without it
/// the active row shows a still ring, which is what a list whose work has
/// stopped should say. Text wraps rather than truncates: a step is a sentence,
/// and its end is often the part that says what it is.
pub fn checklist_row(
    text: impl Into<SharedString>,
    mark: ChecklistMark,
    spin: Option<f32>,
) -> impl IntoElement {
    let theme = crate::theme::global::current();
    let ink = checklist_ink(mark, &theme);
    let role = match mark {
        ChecklistMark::Active => TextRole::CaptionMedium,
        ChecklistMark::Pending | ChecklistMark::Done => TextRole::Caption,
    };
    let line = Typography::style(role, &theme).size * role.line_height_multiplier();
    let slot = Typography::icon(IconSize::Medium, &theme).size;
    let glyph: AnyElement = match (mark, spin) {
        (ChecklistMark::Done, _) => {
            Icon::CHECKMARK_CIRCLE_FILL.render(IconSize::Medium, ink.mark, &theme)
        }
        (ChecklistMark::Active, Some(spin)) => spinner_arc(ink.mark, spin).into_any_element(),
        (ChecklistMark::Active, None) => static_ring(ink.mark).into_any_element(),
        (ChecklistMark::Pending, _) => Icon::CIRCLE.render(IconSize::Medium, ink.mark, &theme),
    };
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_start()
        .gap(px_len(Spacing::SM))
        // A gutter one text line tall, so the mark sits on the first line of a
        // step that wraps and every mark lines up whatever its shape.
        .child(
            div()
                .flex_shrink_0()
                .w(px(slot))
                .h(px(line))
                .flex()
                .items_center()
                .justify_center()
                .child(glyph),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .whitespace_normal()
                .fx_role(role)
                .text_color(ink.text)
                .when(mark == ChecklistMark::Done, |text| text.line_through())
                .child(text.into()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_marks_are_told_apart_by_more_than_their_text() {
        let theme = Theme::default();
        let inks: Vec<ChecklistInk> = ChecklistMark::ALL
            .iter()
            .map(|mark| checklist_ink(*mark, &theme))
            .collect();
        for (i, a) in inks.iter().enumerate() {
            for b in &inks[i + 1..] {
                assert_ne!(a.mark, b.mark, "two marks share an ink");
            }
        }
    }
}
