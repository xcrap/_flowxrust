use gpui::prelude::*;
use gpui::{
    App, ClickEvent, IntoElement, ParentElement, Rgba, SharedString, Styled, Window, div, px,
};

use crate::icon::Icon;
use crate::styled::{FxRefinement, FxStyled, px_len};
use crate::theme::{
    Colors, FxAnimation, IconSize, Spacing, TextRole, Typography, WithAlpha, global,
};

/// One layer centered in a fixed square. Absolute positioning keeps overlaid
/// rings, fills, and glyphs out of flex layout and prevents them shrinking each
/// other into strips when their combined widths exceed the control.
fn centered_layer(outer: f32, inner: f32) -> gpui::Div {
    let inset = (outer - inner) / 2.0;
    div()
        .absolute()
        .left(px(inset))
        .top(px(inset))
        .size(px(inner))
}

/// Semantic tone of a badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeTone {
    #[default]
    Neutral,
    Accent,
    AccentSecondary,
    Success,
    Warning,
    Error,
    Info,
}

impl BadgeTone {
    pub const ALL: [Self; 7] = [
        Self::Neutral,
        Self::Accent,
        Self::AccentSecondary,
        Self::Success,
        Self::Warning,
        Self::Error,
        Self::Info,
    ];

    /// The hue, used for a badge's leading dot and by callers that have already
    /// established a legible background.
    pub fn hue(self, theme: &crate::theme::Theme) -> Rgba {
        match self {
            Self::Neutral => Colors::fg_secondary(theme),
            Self::Accent => Colors::accent(theme),
            Self::AccentSecondary => Colors::accent_secondary(theme),
            Self::Success => Colors::success(theme),
            Self::Warning => Colors::warning(theme),
            Self::Error => Colors::error(theme),
            Self::Info => Colors::info(theme),
        }
    }

    /// The tinted fill behind the label.
    pub fn fill(self, theme: &crate::theme::Theme) -> Rgba {
        match self {
            // Neutral is not a status, so it borrows the selected wash rather
            // than inventing a gray tint that would not adapt per mode.
            Self::Neutral => Colors::bg_selected(theme),
            Self::Accent => Colors::accent_muted(theme),
            Self::AccentSecondary => Colors::accent_secondary_muted(theme),
            Self::Success => Colors::success_muted(theme),
            Self::Warning => Colors::warning_muted(theme),
            Self::Error => Colors::error_muted(theme),
            Self::Info => Colors::info_muted(theme),
        }
    }

    /// The color of a badge's leading dot.
    ///
    /// A dot is a graphic, so it clears the 3:1 non-text bar rather than AA.
    pub fn dot(self, theme: &crate::theme::Theme) -> Rgba {
        match self {
            Self::Neutral => Colors::fg_tertiary(theme),
            other => other.hue(theme),
        }
    }

    /// The color of a badge's *label*.
    ///
    /// A badge is a 12pt caption at medium weight, so its text needs AA
    /// (4.5:1) against its own fill. The hue alone does not manage that: in
    /// light mode `warning` over its own tint lands near 2.6:1, which is
    /// unreadable at that size.
    ///
    /// So the hue is used wherever it is legible, and the strongest foreground
    /// takes over where it is not. This keeps the tint — which is what actually
    /// communicates the tone — while never shipping an illegible label.
    ///
    /// The fill is translucent, so what it reads as depends on what is under
    /// it. A badge sits on a panel as often as on a card, so both surfaces are
    /// measured: judged over the panel alone, the dark "Ready" label on a
    /// provider card was 4.2:1.
    pub fn text_ink(self, theme: &crate::theme::Theme) -> Rgba {
        let fills = [Colors::bg_elevated(theme), Colors::bg_surface(theme)]
            .map(|surface| crate::theme::composite(self.fill(theme), surface));
        crate::theme::legible_or(
            self.hue(theme),
            Colors::fg(theme),
            &fills,
            Self::TEXT_CONTRAST_FLOOR,
        )
    }
}

impl BadgeTone {
    /// AA for text, which is what a badge's label is.
    const TEXT_CONTRAST_FLOOR: f64 = 4.5;
}

/// Diameter of a badge's optional leading dot.
const BADGE_DOT_SIZE: f32 = 6.0;

/// A pill-shaped label, optionally led by a dot.
pub fn badge(text: impl Into<SharedString>, tone: BadgeTone, show_dot: bool) -> impl IntoElement {
    let theme = global::current();
    let mut content = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::XXS));
    if show_dot {
        content = content.child(
            div()
                .size(px_len(BADGE_DOT_SIZE))
                .rounded_full()
                .bg(tone.dot(&theme)),
        );
    }
    content
        .child(div().fx_role(TextRole::CaptionMedium).child(text.into()))
        .px(px_len(Spacing::SM))
        .py(px_len(Spacing::XXXS))
        .bg(tone.fill(&theme))
        .text_color(tone.text_ink(&theme))
        .rounded_full()
}

/// Outer and inner diameters of an activity dot.
const ACTIVITY_OUTER: f32 = 16.0;
const ACTIVITY_INNER: f32 = 7.0;
const ACTIVITY_RING: f32 = 13.0;
const ACTIVITY_STROKE: f32 = 1.5;

/// What an activity dot is reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityState {
    /// Present but doing nothing.
    #[default]
    Idle,
    /// A turn is in flight.
    Running,
    /// Blocked on an approval or a question.
    Waiting,
    /// Finished successfully.
    Completed,
    /// Failed.
    Error,
}

impl ActivityState {
    pub const ALL: [Self; 5] = [
        Self::Idle,
        Self::Running,
        Self::Waiting,
        Self::Completed,
        Self::Error,
    ];

    /// The status ring's color. `None` means the ring is an arc of the identity
    /// color, which is what `Running` draws.
    pub fn ring_color(self, identity: Rgba, theme: &crate::theme::Theme) -> Option<Rgba> {
        match self {
            Self::Idle => Some(identity.with_alpha(0.14)),
            Self::Running => None,
            Self::Waiting => Some(Colors::warning(theme)),
            Self::Completed => Some(Colors::success(theme)),
            Self::Error => Some(Colors::error(theme)),
        }
    }
}

/// A compact identity dot that can also report live activity.
///
/// This keeps a sidebar row to a single line: the dot replaces a second badge or
/// a status label beside the title.
pub fn activity_dot(identity: Rgba, state: ActivityState, spin: f32) -> impl IntoElement {
    let theme = global::current();
    let inner = div()
        .size(px_len(ACTIVITY_INNER))
        .rounded_full()
        .bg(identity);

    let ring: gpui::AnyElement = match state.ring_color(identity, &theme) {
        // A filled disc, sitting behind the identity dot.
        Some(color) => div()
            .size(px_len(ACTIVITY_RING))
            .rounded_full()
            .bg(color)
            .into_any_element(),
        // An arc, drawn as rotated capsules below.
        None => spinner_arc(identity, spin).into_any_element(),
    };

    let ring_size = if state == ActivityState::Running {
        SPINNER_RING_SIZE
    } else {
        ACTIVITY_RING
    };
    div()
        .relative()
        .size(px_len(ACTIVITY_OUTER))
        .flex_shrink_0()
        .debug_selector(|| "activity-dot".into())
        .child(
            centered_layer(ACTIVITY_OUTER, ring_size)
                .debug_selector(|| "activity-ring".into())
                .child(ring),
        )
        .child(
            centered_layer(ACTIVITY_OUTER, ACTIVITY_INNER)
                .debug_selector(|| "activity-inner".into())
                .child(inner),
        )
}

/// Number of segments used to approximate the spinner arc.
const SPINNER_SEGMENTS: usize = 12;
/// Fraction of the circle the arc spans.
const SPINNER_START: f32 = 0.08;
const SPINNER_END: f32 = 0.68;
const SPINNER_RING_SIZE: f32 = 14.0;

/// A partial ring, approximated with rotated capsules.
///
/// `spin` is in degrees. Anything motion-sensitive belongs in the animation
/// system, not here: this function draws one frame.
pub fn spinner_arc(color: Rgba, spin: f32) -> impl IntoElement {
    let radius = (SPINNER_RING_SIZE - ACTIVITY_STROKE) / 2.0;
    let stroke = ACTIVITY_STROKE;
    let center = SPINNER_RING_SIZE / 2.0;
    let span = (SPINNER_END - SPINNER_START).clamp(0.0, 1.0);

    // Lay the capsules out on the circumference rather than rotating one
    // capsule, so the arc has no visible seam as it spins. Only the segments
    // inside [SPINNER_START, SPINNER_END) are drawn, which is what makes this
    // an arc rather than a ring.
    let segments: Vec<_> = (0..SPINNER_SEGMENTS)
        .filter_map(|i| {
            let t = (i as f32 + 0.5) / SPINNER_SEGMENTS as f32;
            if t < SPINNER_START || t > SPINNER_END {
                return None;
            }
            // Progress along the arc, so the trailing end can be faded out.
            let progress = (t - SPINNER_START) / span;
            let angle = (t * 360.0 + spin).to_radians();
            let fade = 0.35 + 0.65 * progress;
            Some(
                div()
                    .absolute()
                    .size(px_len(stroke))
                    .rounded_full()
                    .bg(color.with_alpha(fade))
                    .left(px(center + radius * angle.cos() - stroke / 2.0))
                    .top(px(center + radius * angle.sin() - stroke / 2.0))
                    .into_any_element(),
            )
        })
        .collect();

    div()
        .relative()
        .size(px_len(SPINNER_RING_SIZE))
        .child(div().absolute().size(px_len(SPINNER_RING_SIZE)))
        .children(segments)
        .overflow_hidden()
        .into_any_element()
}

/// A steady ring used when motion is reduced.
pub fn static_ring(color: Rgba) -> impl IntoElement {
    div()
        .size(px_len(SPINNER_RING_SIZE))
        .rounded_full()
        .border_2()
        .border_color(color.with_alpha(0.75))
}

/// One dot of the typing indicator.
const TYPING_DOT_SIZE: f32 = 4.0;

/// Three dots that cycle while the assistant composes a reply.
pub fn typing_indicator(phase: usize, reduce_motion: bool) -> impl IntoElement {
    let theme = global::current();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::XXS))
        .children(
            (0..3)
                .map(|i| {
                    // GPUI cannot scale an element without a transform, so the
                    // emphasis is expressed as a slightly larger dot.
                    div()
                        .size(px_len(
                            TYPING_DOT_SIZE
                                * FxAnimation::typing_dot_scale(i, phase, reduce_motion),
                        ))
                        .rounded_full()
                        .bg(Colors::fg_tertiary(&theme))
                        .opacity(FxAnimation::typing_dot_opacity(i, phase, reduce_motion))
                })
                .collect::<Vec<_>>(),
        )
}

/// Outer ring diameter of the pulsing status indicator.
const PULSE_OUTER: f32 = 18.0;
const PULSE_INNER: f32 = 8.0;

/// A dot that breathes, for a thread that is working but not streaming.
pub fn pulsing_dot(color: Rgba, progress: f32, reduce_motion: bool) -> impl IntoElement {
    let glow_size = FxAnimation::status_pulse_size(progress, reduce_motion);
    div()
        .relative()
        .size(px_len(PULSE_OUTER))
        .flex_shrink_0()
        .child(
            centered_layer(PULSE_OUTER, glow_size)
                .rounded_full()
                .bg(color.with_alpha(FxAnimation::status_pulse_opacity(progress, reduce_motion))),
        )
        .child(
            centered_layer(PULSE_OUTER, PULSE_INNER)
                .rounded_full()
                .bg(color)
                .opacity(FxAnimation::status_pulse_dot_opacity(
                    progress,
                    reduce_motion,
                )),
        )
}

/// Progress fraction at which the context ring escalates its color.
const CONTEXT_WARNING_AT: f32 = 0.75;
const CONTEXT_CRITICAL_AT: f32 = 0.9;

/// The color a context-usage ring adopts at a given fill level.
pub fn context_ring_color(progress: Option<f32>, theme: &crate::theme::Theme) -> Rgba {
    match progress {
        None => Colors::accent(theme),
        Some(p) if p >= CONTEXT_CRITICAL_AT => Colors::error(theme),
        Some(p) if p >= CONTEXT_WARNING_AT => Colors::warning(theme),
        Some(_) => Colors::accent(theme),
    }
}

/// A filled action button wrapped by a context-usage ring.
///
/// This is the composer's send control: the ring around it reports how much of
/// the model's context the conversation has used, so running long is visible
/// before it becomes a failure.
pub fn context_ring_icon(
    icon: Icon,
    progress: Option<f32>,
    tint: Rgba,
    size: f32,
    enabled: bool,
    hovered: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = global::current();
    let clamped = progress.map(|p| p.clamp(0.0, 1.0));
    // An empty ring is indistinguishable from the track, so the progress arc
    // only appears once there is some.
    let shows_progress = clamped.is_some_and(|p| p > 0.0);
    let fill = if !enabled {
        Colors::bg_hover(&theme)
    } else if hovered {
        tint.with_alpha(0.86)
    } else {
        tint
    };
    let glyph = if enabled {
        Colors::on_accent(&theme)
    } else {
        Colors::fg_quaternary(&theme)
    };

    // The track, the progress arc, the button, then the glyph.
    let ring = div()
        .relative()
        .size(px_len(size))
        .flex_shrink_0()
        .debug_selector(|| "context-ring".into())
        .child(
            centered_layer(size, size)
                .debug_selector(|| "context-ring-track".into())
                .rounded_full()
                .border_2()
                .border_color(Colors::border_medium(&theme)),
        )
        .child(
            centered_layer(size, size)
                .rounded_full()
                .border_2()
                .border_color(
                    context_ring_color(clamped, &theme).with_alpha(if shows_progress {
                        1.0
                    } else {
                        0.0
                    }),
                )
                .opacity(if shows_progress { 1.0 } else { 0.0 }),
        )
        .child(
            centered_layer(size, size - Spacing::SM)
                .debug_selector(|| "context-ring-fill".into())
                .rounded_full()
                .bg(fill),
        )
        .child(
            centered_layer(size, Typography::icon(IconSize::Control, &theme).size)
                .debug_selector(|| "context-ring-glyph".into())
                .child(icon.render(IconSize::Control, glyph, &theme)),
        );

    ring
        // `.id` upgrades to `Stateful<Div>`, which carries `on_click`.
        .id("context-ring-action")
        .hover(|s| {
            if enabled && hovered {
                s.fx_opacity(1.0)
            } else {
                s
            }
        })
        .on_click(move |event, window, cx| {
            if enabled {
                on_click(event, window, cx);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{AppearanceMode, BaseTone, Theme, contrast};

    fn themes() -> Vec<Theme> {
        let mut out = Vec::new();
        for tone in BaseTone::ALL {
            for mode in [AppearanceMode::Light, AppearanceMode::Dark] {
                out.push(Theme {
                    base_tone: tone,
                    appearance_mode: mode,
                    ..Default::default()
                });
            }
        }
        out
    }

    #[test]
    fn badge_hues_are_distinguishable_within_a_mode() {
        for theme in themes() {
            let hues: Vec<_> = BadgeTone::ALL.iter().map(|t| t.hue(&theme)).collect();
            for (i, a) in hues.iter().enumerate() {
                for (j, b) in hues.iter().enumerate().skip(i + 1) {
                    assert_ne!(
                        a,
                        b,
                        "{:?} and {:?} share a hue in {:?}",
                        BadgeTone::ALL[i],
                        BadgeTone::ALL[j],
                        theme.appearance_mode
                    );
                }
            }
        }
    }

    /// A tone's fill over each surface a badge is drawn on, as it renders.
    fn fills(tone: BadgeTone, theme: &Theme) -> [Rgba; 2] {
        [Colors::bg_elevated(theme), Colors::bg_surface(theme)]
            .map(|surface| crate::theme::composite(tone.fill(theme), surface))
    }

    #[test]
    fn badge_text_stays_legible_on_its_own_fill() {
        // A badge label is a 12pt caption, so it needs AA. This is the check
        // that motivated `text_ink` falling back to a strong foreground. The
        // provider rows in Settings put badges on a card, where the dark
        // "Ready" label measured over the panel alone was 4.2:1.
        for theme in themes() {
            for tone in BadgeTone::ALL {
                for fill in fills(tone, &theme) {
                    let ratio = contrast(tone.text_ink(&theme), fill);
                    assert!(
                        ratio >= 4.5,
                        "{tone:?} badge text is {ratio}:1 in {:?} {:?}",
                        theme.base_tone,
                        theme.appearance_mode
                    );
                }
            }
        }
    }

    #[test]
    fn the_text_fallback_keeps_the_hue_wherever_the_hue_is_legible() {
        // The fallback must not flatten every badge to gray. Where the hue is
        // legible on both surfaces, it is kept.
        let theme = Theme {
            appearance_mode: AppearanceMode::Dark,
            ..Default::default()
        };
        let mut kept = 0;
        for tone in BadgeTone::ALL {
            if fills(tone, &theme)
                .iter()
                .all(|fill| contrast(tone.hue(&theme), *fill) >= 4.5)
            {
                assert_eq!(
                    tone.text_ink(&theme),
                    tone.hue(&theme),
                    "{tone:?} could have kept its hue"
                );
                kept += 1;
            }
        }
        // Neutral and warning. The other dark tints are legible on a panel but
        // not on a card, and a badge does not know which one it is on.
        assert!(kept >= 2, "only {kept} tones kept their hue in dark mode");
    }

    #[test]
    fn the_neutral_badge_keeps_its_hue_in_both_modes() {
        // Neutral reads `fg_secondary`, which is built to clear AA everywhere.
        for theme in themes() {
            assert_eq!(
                BadgeTone::Neutral.text_ink(&theme),
                BadgeTone::Neutral.hue(&theme)
            );
        }
    }

    #[test]
    fn every_badge_tone_has_a_translucent_fill_except_neutral() {
        for theme in themes() {
            for tone in BadgeTone::ALL {
                let fill = tone.fill(&theme);
                if tone == BadgeTone::Neutral {
                    // Neutral borrows `bg_selected`, which is a fixed alpha.
                    assert!(fill.a < 1.0);
                } else {
                    assert!(fill.a > 0.0 && fill.a < 1.0, "{tone:?}");
                }
            }
        }
    }

    #[test]
    fn the_neutral_badge_dot_differs_from_its_text() {
        // Both use foreground tokens; keeping them apart stops the pill from
        // reading as one flat shape.
        let theme = Theme::default();
        assert_ne!(
            BadgeTone::Neutral.dot(&theme),
            BadgeTone::Neutral.text_ink(&theme)
        );
    }

    #[test]
    fn only_the_running_state_draws_an_arc() {
        let theme = Theme::default();
        let identity = Colors::accent(&theme);
        assert!(
            ActivityState::Running
                .ring_color(identity, &theme)
                .is_none()
        );
        for state in [
            ActivityState::Idle,
            ActivityState::Waiting,
            ActivityState::Completed,
            ActivityState::Error,
        ] {
            assert!(
                state.ring_color(identity, &theme).is_some(),
                "{state:?} should use a filled ring"
            );
        }
    }

    #[test]
    fn the_status_rings_use_the_expected_semantic_hues() {
        let theme = Theme::default();
        let identity = Colors::accent(&theme);
        assert_eq!(
            ActivityState::Waiting.ring_color(identity, &theme),
            Some(Colors::warning(&theme))
        );
        assert_eq!(
            ActivityState::Completed.ring_color(identity, &theme),
            Some(Colors::success(&theme))
        );
        assert_eq!(
            ActivityState::Error.ring_color(identity, &theme),
            Some(Colors::error(&theme))
        );
    }

    #[test]
    fn the_idle_ring_is_a_faint_wash_of_the_identity_color() {
        let theme = Theme::default();
        let identity = Colors::accent(&theme);
        let ring = ActivityState::Idle.ring_color(identity, &theme).unwrap();
        assert_eq!(
            (ring.r, ring.g, ring.b),
            (identity.r, identity.g, identity.b)
        );
        assert!(
            ring.a < 0.3,
            "the idle ring should be a wash, got {}",
            ring.a
        );
    }

    #[test]
    fn the_spinner_arc_covers_its_documented_fraction_of_the_circle() {
        let span = SPINNER_END - SPINNER_START;
        assert!(
            (0.5..0.7).contains(&span),
            "the arc should cover roughly three fifths of the circle, was {span}"
        );
    }

    #[test]
    fn the_arc_is_drawn_with_enough_segments_to_look_smooth() {
        // Twelve capsules is indistinguishable from a trimmed stroke at 14pt.
        // Fewer would show as facets; more would cost draw calls for nothing.
        assert_eq!(
            SPINNER_SEGMENTS, 12,
            "changing the segment count changes the arc's smoothness"
        );
        // At least one segment must land inside the arc, or nothing renders.
        let inside = (0..SPINNER_SEGMENTS)
            .filter(|i| {
                let t = (*i as f32 + 0.5) / SPINNER_SEGMENTS as f32;
                (SPINNER_START..=SPINNER_END).contains(&t)
            })
            .count();
        assert!(inside >= 5, "only {inside} segments land inside the arc");
    }

    #[test]
    fn the_context_ring_escalates_as_the_window_fills() {
        let theme = Theme::default();
        let empty = context_ring_color(Some(0.1), &theme);
        let high = context_ring_color(Some(0.8), &theme);
        let full = context_ring_color(Some(0.95), &theme);

        assert_eq!(empty, Colors::accent(&theme));
        assert_eq!(high, Colors::warning(&theme));
        assert_eq!(full, Colors::error(&theme));
    }

    #[test]
    fn an_unknown_context_level_falls_back_to_the_accent() {
        let theme = Theme::default();
        assert_eq!(context_ring_color(None, &theme), Colors::accent(&theme));
    }

    #[test]
    fn the_typing_indicator_always_lights_exactly_one_dot() {
        // The rhythm depends on this staying at three; changing the count
        // changes how the indicator reads.
        const DOTS: usize = 3;
        for phase in 0..DOTS {
            let lit = (0..DOTS)
                .filter(|i| FxAnimation::typing_dot_opacity(*i, phase, false) > 0.5)
                .count();
            assert_eq!(lit, 1, "phase {phase} lit {lit} dots");
        }
    }
}
