//! The composer's send/stop control, including its interruptible visual feedback.

use std::time::Instant;

use gpui::prelude::*;
use gpui::{App, ClickEvent, IntoElement, Rgba, SharedString, Window, div, px};

use crate::icon::Icon;
use crate::styled::FxRefinement;
use crate::theme::{
    BorderWidth, Colors, FxAnimation, IconSize, Layout, Radii, Spacing, Theme, Typography, global,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnAction {
    Send,
    Stop,
}

/// A stable hit target whose action changes immediately, while the chrome and
/// glyph cross-fade. State belongs to the element, so function views can use it
/// without adding pointer state to their application models.
pub fn turn_action_button(
    id: impl Into<SharedString>,
    action: TurnAction,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    TurnActionButton {
        id: id.into(),
        action,
        enabled,
        on_click: Box::new(on_click),
    }
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
struct TurnActionButton {
    id: SharedString,
    action: TurnAction,
    enabled: bool,
    on_click: ClickHandler,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Visuals {
    foreground: Rgba,
    background: Rgba,
    border: Rgba,
    stop: f32,
}

impl Visuals {
    fn mix(self, target: Self, progress: f32) -> Self {
        let mix = |from: f32, to: f32| from + (to - from) * progress;
        let color = |from: Rgba, to: Rgba| Rgba {
            r: mix(from.r, to.r),
            g: mix(from.g, to.g),
            b: mix(from.b, to.b),
            a: mix(from.a, to.a),
        };
        Self {
            foreground: color(self.foreground, target.foreground),
            background: color(self.background, target.background),
            border: color(self.border, target.border),
            stop: mix(self.stop, target.stop),
        }
    }
}

fn palette(action: TurnAction, enabled: bool, hovered: bool, theme: &Theme) -> Visuals {
    let stop = if action == TurnAction::Stop { 1.0 } else { 0.0 };
    if action == TurnAction::Send && enabled {
        let background = if hovered {
            Colors::accent_hover(theme)
        } else {
            Colors::accent(theme)
        };
        Visuals {
            foreground: Colors::on_accent(theme),
            background,
            border: background,
            stop,
        }
    } else {
        Visuals {
            foreground: if enabled {
                Colors::fg(theme)
            } else {
                Colors::fg_tertiary(theme)
            },
            background: if enabled && hovered {
                Colors::bg_hover(theme)
            } else {
                Colors::bg_surface(theme)
            },
            border: if enabled && hovered {
                Colors::border_medium(theme)
            } else {
                Colors::border(theme)
            },
            stop,
        }
    }
}

struct Feedback {
    hovered: bool,
    from: Visuals,
    target: Visuals,
    started: Instant,
}

impl Feedback {
    fn new(visuals: Visuals, now: Instant) -> Self {
        Self {
            hovered: false,
            from: visuals,
            target: visuals,
            started: now,
        }
    }

    fn current(&self, now: Instant) -> Visuals {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= FxAnimation::QUICK || self.from == self.target {
            return self.target;
        }
        let progress = elapsed.as_secs_f32() / FxAnimation::QUICK.as_secs_f32();
        self.from
            .mix(self.target, FxAnimation::ease_out()(progress))
    }

    fn retarget(&mut self, target: Visuals, now: Instant) {
        if target != self.target {
            self.from = self.current(now);
            self.target = target;
            self.started = now;
        }
    }

    fn is_animating(&self, now: Instant) -> bool {
        self.from != self.target && now.saturating_duration_since(self.started) < FxAnimation::QUICK
    }
}

impl RenderOnce for TurnActionButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = global::current();
        let now = Instant::now();
        let state = window.with_element_namespace(self.id.clone(), |window| {
            window.use_keyed_state("feedback", cx, |_, _| {
                Feedback::new(palette(self.action, self.enabled, false, &theme), now)
            })
        });
        let visuals = state.update(cx, |feedback, _| {
            feedback.retarget(
                palette(self.action, self.enabled, feedback.hovered, &theme),
                now,
            );
            if feedback.is_animating(now) {
                // RenderOnce runs in the element's layout phase, where GPUI
                // has a current view and can schedule its next paint safely.
                window.request_animation_frame();
            }
            feedback.current(now)
        });
        let label = match self.action {
            TurnAction::Send => "Send this prompt",
            TurnAction::Stop => "Stop this turn",
        };
        let enabled = self.enabled;
        let pressed = if self.action == TurnAction::Send {
            Colors::accent_hover(&theme)
        } else {
            Colors::bg_pressed(&theme)
        };

        div()
            .id(self.id)
            .size(px(Layout::COMPOSER_ACTION_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radii::LG))
            .border(px(BorderWidth::HAIRLINE))
            .border_color(visuals.border)
            .bg(visuals.background)
            .cursor(if enabled {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::default()
            })
            .active(move |style| {
                if enabled {
                    style.fx_background(pressed).fx_opacity(0.94)
                } else {
                    style
                }
            })
            .on_hover(move |hovered, _, cx| {
                state.update(cx, |feedback, cx| {
                    if feedback.hovered != *hovered {
                        feedback.hovered = *hovered;
                        cx.notify();
                    }
                });
            })
            .on_click(move |event, window, cx| {
                if enabled {
                    (self.on_click)(event, window, cx);
                }
            })
            .tooltip(move |window, cx| super::tooltip(label, window, cx))
            .child(
                div()
                    .relative()
                    .size(px(Typography::icon(IconSize::Large, &theme).size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .opacity(1.0 - visuals.stop)
                            .child(Icon::ARROW_UP.render(
                                IconSize::Large,
                                visuals.foreground,
                                &theme,
                            )),
                    )
                    .child(
                        div()
                            .debug_selector(|| "turn-action-stop-glyph".into())
                            .size(px(Spacing::MD))
                            .rounded(px(Spacing::XXXS))
                            .bg(visuals.foreground)
                            .opacity(visuals.stop),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AppearanceMode, BaseTone};

    #[test]
    fn both_actions_have_hover_feedback_in_every_theme_and_disabled_does_not() {
        for base_tone in BaseTone::ALL {
            for appearance in [AppearanceMode::Light, AppearanceMode::Dark] {
                let theme = Theme {
                    base_tone,
                    appearance_mode: appearance,
                    ..Theme::default()
                };
                for action in [TurnAction::Send, TurnAction::Stop] {
                    assert_ne!(
                        palette(action, true, false, &theme),
                        palette(action, true, true, &theme)
                    );
                    assert_eq!(
                        palette(action, false, false, &theme),
                        palette(action, false, true, &theme)
                    );
                }
            }
        }
    }

    #[test]
    fn hover_and_turn_changes_retarget_from_the_visible_state_and_settle() {
        let theme = Theme::default();
        let now = Instant::now();
        let send = palette(TurnAction::Send, true, false, &theme);
        let hovered = palette(TurnAction::Send, true, true, &theme);
        let stop = palette(TurnAction::Stop, true, true, &theme);
        let mut feedback = Feedback::new(send, now);
        assert!(!feedback.is_animating(now));
        feedback.retarget(hovered, now);
        let halfway = now + FxAnimation::QUICK / 2;
        let visible = feedback.current(halfway);
        assert_ne!(visible, send);
        assert_ne!(visible, hovered);
        feedback.retarget(stop, halfway);
        assert_eq!(feedback.current(halfway), visible);
        let middle = feedback.current(halfway + FxAnimation::QUICK / 2);
        assert!(middle.stop > 0.0 && middle.stop < 1.0);
        assert_eq!(feedback.current(halfway + FxAnimation::QUICK), stop);
        assert!(!feedback.is_animating(halfway + FxAnimation::QUICK));
    }
}
