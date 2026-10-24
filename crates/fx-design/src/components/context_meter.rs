//! A small context gauge, independent of the send/stop action.

use gpui::prelude::*;
use gpui::{PathBuilder, canvas, div, point, px};

use super::ProviderIdentity;
use crate::styled::FxStyled;
use crate::theme::{Colors, Layout, Radii, Spacing, TextRole, global};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextUsage {
    /// None means the provider has not reported usage, not an empty context.
    pub used: Option<u64>,
    pub limit: Option<u64>,
}

impl ContextUsage {
    pub fn fraction(self) -> Option<f32> {
        Some(
            (self.used? as f64 / self.limit.filter(|limit| *limit > 0)? as f64).clamp(0.0, 1.0)
                as f32,
        )
    }

    pub fn label(self) -> String {
        self.fraction()
            .map(|fraction| format!("{:.0}%", fraction * 100.0))
            .unwrap_or_else(|| "—".into())
    }

    pub fn detail(self) -> String {
        let usage = self
            .used
            .map(tokens)
            .unwrap_or_else(|| "Usage not reported yet".into());
        let limit = self
            .limit
            .filter(|limit| *limit > 0)
            .map(tokens)
            .unwrap_or_else(|| "Unknown limit".into());
        let percent = self
            .fraction()
            .map(|_| format!(" · {} used", self.label()))
            .unwrap_or_default();
        format!("Context: {usage} / {limit}{percent}")
    }
}

fn tokens(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} tokens")
}

pub fn context_meter(usage: ContextUsage, provider: ProviderIdentity) -> impl IntoElement {
    let theme = global::current();
    let fraction = usage.fraction();
    let ink = match fraction {
        Some(f) if f >= 0.9 => Colors::error(&theme),
        Some(f) if f >= 0.75 => Colors::warning(&theme),
        _ => provider.color(&theme),
    };
    let track = Colors::border_medium(&theme);
    let detail = usage.detail();
    div()
        .id("composer.context")
        .debug_selector(|| "composer-context".into())
        .h(px(Layout::CONTROL_COMPACT_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(Spacing::XXS))
        .px(px(Spacing::XXS))
        .rounded(px(Radii::SM))
        .fx_role(TextRole::Caption)
        .text_color(Colors::fg_secondary(&theme))
        .whitespace_nowrap()
        .tooltip(move |window, cx| super::tooltip(detail.clone(), window, cx))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let center = bounds.center();
                    let radius = bounds.size.width / 2.0 - px(1.0);
                    for (amount, color) in [(1.0, track), (fraction.unwrap_or(0.0), ink)] {
                        if amount <= 0.0 {
                            continue;
                        }
                        let mut path = PathBuilder::stroke(px(1.5));
                        let steps = (64.0 * amount).ceil() as usize;
                        for step in 0..=steps {
                            let angle = -std::f32::consts::FRAC_PI_2
                                + std::f32::consts::TAU * amount * step as f32 / steps as f32;
                            let position = point(
                                center.x + radius * angle.cos(),
                                center.y + radius * angle.sin(),
                            );
                            if step == 0 {
                                path.move_to(position);
                            } else {
                                path.line_to(position);
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                    }
                },
            )
            .size(px(Spacing::MD)),
        )
        .child(usage.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_usage_is_not_an_empty_context_and_zero_is_not_a_limit() {
        assert_eq!(
            ContextUsage {
                used: None,
                limit: Some(200_000)
            }
            .label(),
            "—"
        );
        assert_eq!(
            ContextUsage {
                used: Some(0),
                limit: Some(200_000)
            }
            .label(),
            "0%"
        );
        assert_eq!(
            ContextUsage {
                used: Some(1),
                limit: Some(0)
            }
            .fraction(),
            None
        );
    }

    #[test]
    fn the_gauge_clamps_but_its_tooltip_preserves_the_reported_counts() {
        let usage = ContextUsage {
            used: Some(234_567),
            limit: Some(200_000),
        };
        assert_eq!(usage.fraction(), Some(1.0));
        assert_eq!(
            usage.detail(),
            "Context: 234,567 tokens / 200,000 tokens · 100% used"
        );
        assert_eq!(
            ContextUsage {
                used: Some(50_000),
                ..usage
            }
            .fraction(),
            Some(0.25)
        );
    }
}
