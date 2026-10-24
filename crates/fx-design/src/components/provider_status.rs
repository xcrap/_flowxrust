//! Provider identity and live activity in one compact sidebar tile.

use std::time::Instant;

use gpui::prelude::*;
use gpui::{AnyElement, App, IntoElement, Rgba, SharedString, Window, div, px};

use super::{ActivityState, spinner_arc};
use crate::icon::Icon;
use crate::theme::{
    BorderWidth, Colors, FxAnimation, IconSize, Radii, Spacing, Theme, WithAlpha, global,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderIdentity {
    #[default]
    Codex,
    Claude,
}

impl ProviderIdentity {
    pub fn color(self, theme: &Theme) -> Rgba {
        match self {
            Self::Codex => Colors::provider_codex(theme),
            Self::Claude => Colors::provider_claude(theme),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }
}

pub fn provider_status(
    id: impl Into<SharedString>,
    provider: ProviderIdentity,
    activity: ActivityState,
    label: &'static str,
) -> impl IntoElement {
    ProviderStatus {
        id: id.into(),
        provider,
        activity,
        label,
    }
}

#[derive(IntoElement)]
struct ProviderStatus {
    id: SharedString,
    provider: ProviderIdentity,
    activity: ActivityState,
    label: &'static str,
}

impl RenderOnce for ProviderStatus {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = global::current();
        let identity = self.provider.color(&theme);
        let (clock, scheduled) = window.with_element_namespace(self.id.clone(), |window| {
            (
                window.use_keyed_state("activity-clock", cx, |_, _| Instant::now()),
                window.use_keyed_state("activity-frame", cx, |_, _| false),
            )
        });
        let elapsed = clock.read(cx).elapsed().as_secs_f32();
        let waiting = self.activity == ActivityState::Waiting;
        let running = self.activity == ActivityState::Running;
        // One timed repaint at a time, not a frame per display refresh. Settled
        // tiles schedule nothing, so motion ends as soon as the state does.
        if (running || waiting) && !*scheduled.read(cx) {
            scheduled.update(cx, |scheduled, _| *scheduled = true);
            window
                .spawn(cx, async move |cx| {
                    // The platform's timer, which a test clock can advance.
                    cx.background_executor()
                        .timer(FxAnimation::ACTIVITY_FRAME)
                        .await;
                    let _ = cx.update(|window, cx| {
                        scheduled.update(cx, |scheduled, _| *scheduled = false);
                        window.refresh();
                    });
                })
                .detach();
        }
        let pulse = if waiting {
            let phase = elapsed / FxAnimation::STATUS_PULSE.as_secs_f32();
            0.5 + 0.5 * (phase * std::f32::consts::TAU).cos()
        } else {
            1.0
        };
        let ink = if self.activity == ActivityState::Error {
            Colors::error(&theme)
        } else {
            identity
        };
        let glyph: AnyElement = match self.activity {
            ActivityState::Running => spinner_arc(
                identity,
                elapsed / FxAnimation::ACTIVITY_SPIN.as_secs_f32() * 360.0,
            )
            .into_any_element(),
            ActivityState::Completed => Icon::CHECKMARK
                .render(IconSize::Small, ink, &theme)
                .into_any_element(),
            ActivityState::Error => Icon::XMARK
                .render(IconSize::Small, ink, &theme)
                .into_any_element(),
            ActivityState::Waiting => div()
                .flex()
                .gap(px(Spacing::XXXS))
                .children((0..2).map(|_| {
                    div()
                        .w(px(Spacing::XXXS))
                        .h(px(Spacing::SM))
                        .rounded(px(Radii::XS))
                        .bg(ink)
                }))
                .into_any_element(),
            ActivityState::Idle => div()
                .size(px(Spacing::XXS))
                .rounded_full()
                .bg(ink)
                .into_any_element(),
        };
        let detail = format!("{} · {}", self.provider.label(), self.label);
        div()
            .id(self.id)
            .debug_selector(|| "provider-status".into())
            .size(px(Spacing::XL))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radii::SM))
            .border(px(BorderWidth::HAIRLINE))
            .border_color(ink.with_alpha(if waiting { 0.3 + 0.4 * pulse } else { 0.25 }))
            .bg(identity.with_alpha(if waiting { 0.08 + 0.1 * pulse } else { 0.12 }))
            .tooltip(move |window, cx| super::tooltip(detail.clone(), window, cx))
            .child(glyph)
    }
}
