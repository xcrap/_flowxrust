//! Styled inline text that shapes and wraps as a single paragraph.
//!
//! Keeping styles in text runs, rather than one element per word or link, is
//! what lets punctuation, code and links share the same baseline and wrap.

use std::ops::Range;

use gpui::{
    FontStyle, FontWeight, SharedString, StrikethroughStyle, StyledText, TextRun, UnderlineStyle,
    font, px,
};

use crate::theme::{BorderWidth, Colors, FAMILY_MONO, TextRole, Theme, Typography};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InlineStyle {
    pub strong: bool,
    pub emphasis: bool,
    pub code: bool,
    pub strikethrough: bool,
    pub link: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    /// UTF-8 byte range. Spans cover the text in order without overlap.
    pub range: Range<usize>,
    pub style: InlineStyle,
}

pub fn inline_run(span: &InlineSpan, role: TextRole, theme: &Theme) -> TextRun {
    let typography = Typography::style(role, theme);
    let mut font = font(if span.style.code {
        FAMILY_MONO
    } else {
        typography.family
    });
    font.weight = if span.style.strong {
        FontWeight::SEMIBOLD
    } else {
        typography.weight
    };
    font.style = if span.style.emphasis {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    TextRun {
        len: span.range.len(),
        font,
        color: if span.style.link {
            Colors::accent_secondary(theme)
        } else {
            Colors::fg(theme)
        }
        .into(),
        background_color: span.style.code.then(|| Colors::bg_surface(theme).into()),
        underline: span.style.link.then_some(UnderlineStyle {
            thickness: px(BorderWidth::REGULAR),
            color: None,
            wavy: false,
        }),
        strikethrough: span.style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(BorderWidth::REGULAR),
            color: None,
        }),
    }
}

pub fn rich_text(
    text: impl Into<SharedString>,
    spans: &[InlineSpan],
    role: TextRole,
    theme: &Theme,
) -> StyledText {
    StyledText::new(text).with_runs(
        spans
            .iter()
            .map(|span| inline_run(span, role, theme))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_link_code_and_emphasis_keep_their_independent_styles() {
        let theme = Theme::default();
        let span = InlineSpan {
            range: 0..8,
            style: InlineStyle {
                strong: true,
                emphasis: true,
                code: true,
                link: true,
                strikethrough: false,
            },
        };
        let run = inline_run(&span, TextRole::Body, &theme);
        assert_eq!(run.len, 8);
        assert_eq!(run.font.family.as_ref(), FAMILY_MONO);
        assert_eq!(run.font.weight, FontWeight::SEMIBOLD);
        assert_eq!(run.font.style, FontStyle::Italic);
        assert_eq!(run.color, Colors::accent_secondary(&theme).into());
        assert!(run.underline.is_some());
        assert!(run.background_color.is_some());
    }
}
