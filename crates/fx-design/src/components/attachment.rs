//! Bounded attachment previews shared by composer and transcript surfaces.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{AnyElement, ObjectFit, RenderImage, SharedString, StyledImage, div, img, px};

use crate::icon::Icon;
use crate::styled::{FxStyled, px_len};
use crate::theme::{BorderWidth, Colors, IconSize, Radii, Spacing, global};

pub const IMAGE_PREVIEW_MAX_WIDTH: f32 = 320.0;
pub const IMAGE_PREVIEW_MAX_HEIGHT: f32 = 220.0;

/// Keep the source's proportions and never enlarge a small attachment.
pub fn image_preview_size(width: u32, height: u32) -> (f32, f32) {
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let scale = (IMAGE_PREVIEW_MAX_WIDTH / width)
        .min(IMAGE_PREVIEW_MAX_HEIGHT / height)
        .min(1.0);
    (width * scale, height * scale)
}

pub fn image_preview(image: Arc<RenderImage>, width: u32, height: u32) -> AnyElement {
    let theme = global::current();
    let (width, height) = image_preview_size(width, height);
    div()
        .debug_selector(|| "attachment.preview".into())
        .w(px(width))
        .h(px(height))
        .max_w_full()
        .min_w_0()
        .flex_shrink_0()
        .overflow_hidden()
        .rounded(px_len(Radii::MD))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .bg(Colors::bg_surface(&theme))
        .child(img(image).size_full().object_fit(ObjectFit::Contain))
        .into_any_element()
}

/// Missing or invalid images occupy one compact row, never the source's height.
pub fn attachment_fallback(
    label: impl Into<SharedString>,
    detail: impl Into<SharedString>,
) -> AnyElement {
    let theme = global::current();
    div()
        .debug_selector(|| "attachment.fallback".into())
        .w(px(IMAGE_PREVIEW_MAX_WIDTH))
        .max_w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .rounded(px_len(Radii::MD))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .bg(Colors::bg_surface(&theme))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .child(div().flex_shrink_0().child(Icon::PHOTO.render(
            IconSize::Regular,
            Colors::fg_tertiary(&theme),
            &theme,
        )))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .fx_caption()
                        .text_color(Colors::fg(&theme))
                        .child(label.into()),
                )
                .child(
                    div()
                        .truncate()
                        .fx_caption()
                        .text_color(Colors::fg_secondary(&theme))
                        .child(detail.into()),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_landscape_and_tiny_previews_keep_their_proportions() {
        for (width, height) in [(1858, 2488), (4000, 1000), (1, 1), (120, 40)] {
            let (w, h) = image_preview_size(width, height);
            assert!(w <= IMAGE_PREVIEW_MAX_WIDTH && h <= IMAGE_PREVIEW_MAX_HEIGHT);
            assert!(w <= width as f32 && h <= height as f32);
            assert!((w / h - width as f32 / height as f32).abs() < 0.001);
        }
    }
}
