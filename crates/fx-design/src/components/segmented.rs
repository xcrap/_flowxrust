use std::rc::Rc;

use gpui::prelude::*;
use gpui::{App, CursorStyle, IntoElement, ParentElement, SharedString, Styled, Window, div};

use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{Colors, Radii, Spacing, TextRole, global};

/// Height of a segment.
const SEGMENT_HEIGHT: f32 = 24.0;

/// The callback every segment of a segmented control shares.
///
/// Takes the value by reference to match the shape of a GPUI
/// `Context::listener`, so a control can be wired straight to one.
type SelectionHandler<T> = Rc<dyn Fn(&T, &mut Window, &mut App)>;

/// One choice in a segmented control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentedOption<T> {
    pub value: T,
    pub title: SharedString,
}

impl<T> SegmentedOption<T> {
    pub fn new(value: T, title: impl Into<SharedString>) -> Self {
        Self {
            value,
            title: title.into(),
        }
    }
}

/// A row of exclusive choices, with the selected one filled.
///
/// Sized to its content rather than stretched, so a two-option control in a
/// toolbar does not read as a full-width field.
pub fn segmented_control<T: PartialEq + Clone + 'static>(
    options: &[SegmentedOption<T>],
    selected: &T,
    on_select: impl Fn(&T, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();

    // Every segment needs the same callback, so share one copy rather than
    // requiring the caller's closure to be cloneable.
    let on_select: SelectionHandler<T> = Rc::new(on_select);

    let segments = options.iter().map(|option| {
        let is_selected = &option.value == selected;
        let value = option.value.clone();
        div()
            .px(px_len(Spacing::SM))
            .h(px_len(SEGMENT_HEIGHT))
            .flex_col()
            .justify_center()
            .rounded(px_len(Radii::XS))
            .bg(if is_selected {
                Colors::bg_selected(&theme)
            } else {
                gpui::Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.0,
                }
            })
            .text_color(if is_selected {
                Colors::fg(&theme)
            } else {
                Colors::fg_tertiary(&theme)
            })
            .fx_role(TextRole::CaptionMedium)
            .child(option.title.clone())
            // `.id` upgrades to `Stateful<Div>`, which carries `on_click`.
            .id(gpui::SharedString::from(format!(
                "segment:{}",
                option.title
            )))
            .cursor(CursorStyle::PointingHand)
            .hover(|s| {
                if is_selected {
                    s
                } else {
                    s.fx_background(Colors::bg_hover(&theme))
                        .fx_text_color(Colors::fg_secondary(&theme))
                }
            })
            .on_click({
                let on_select = Rc::clone(&on_select);
                move |_event, window, cx| on_select(&value, window, cx)
            })
    });

    div()
        .flex()
        .flex_row()
        .gap(px_len(Spacing::XXXS))
        .p(px_len(Spacing::XXXS))
        .bg(Colors::bg_surface(&theme))
        .rounded(px_len(Radii::SM))
        .children(segments.collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_option_carries_its_value_and_title() {
        let o = SegmentedOption::new(2u8, "Two");
        assert_eq!(o.value, 2);
        assert_eq!(o.title, "Two");
    }

    #[test]
    fn options_compare_by_value_not_title() {
        // Two options can share a label while meaning different things.
        let a = SegmentedOption::new(1u8, "Same");
        let b = SegmentedOption::new(2u8, "Same");
        assert_ne!(a, b);
    }
}
