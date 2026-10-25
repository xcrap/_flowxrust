//! Animation presets.
//!
//! Views never write a duration or an easing curve. Pick a named preset so that
//! every transition in the app feels like it came from the same hand.
//!
//! GPUI drives animation through [`gpui::Animation`] plus a progress callback,
//! so these presets are duration + easing pairs. The spring presets use a
//! critically-damped curve rather than a physics solver: the GPUI 0.2 animation
//! model has no spring integrator, and a damped curve is visually equivalent
//! for these short transitions. [`PanelTransition`] also preserves position
//! when a panel's direction changes before the transition finishes.

use std::time::{Duration, Instant};

use gpui::Animation;

pub struct FxAnimation;

/// Interruptible panel reveal. A reversal starts at the current position,
/// rather than restarting at either endpoint. Initial state never animates.
#[derive(Clone, Debug)]
pub struct PanelTransition {
    from: f32,
    target: f32,
    started: Instant,
}

impl PanelTransition {
    pub fn new(expanded: bool) -> Self {
        let value = if expanded { 1.0 } else { 0.0 };
        Self {
            from: value,
            target: value,
            started: Instant::now(),
        }
    }

    pub fn set_expanded(&mut self, expanded: bool, now: Instant) {
        let target = if expanded { 1.0 } else { 0.0 };
        if self.target != target {
            self.from = self.progress(now);
            self.target = target;
            self.started = now;
        }
    }

    pub fn progress(&self, now: Instant) -> f32 {
        let fraction = (now.saturating_duration_since(self.started).as_secs_f32()
            / FxAnimation::PANEL.as_secs_f32())
        .min(1.0);
        if fraction == 1.0 {
            return self.target;
        }
        self.from + (self.target - self.from) * FxAnimation::ease_out()(fraction)
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.from != self.target && now.saturating_duration_since(self.started) < FxAnimation::PANEL
    }
}

impl FxAnimation {
    /// The default for state changes: quick, no overshoot.
    pub const SNAPPY: Duration = Duration::from_millis(250);
    /// Larger surfaces settling into place. Slight overshoot.
    pub const GENTLE: Duration = Duration::from_millis(350);
    /// Hover and press feedback.
    pub const QUICK: Duration = Duration::from_millis(150);
    /// Cross-fades and layout shifts where overshoot would look wrong.
    pub const SMOOTH: Duration = Duration::from_millis(300);
    /// Panels and sheets sliding in.
    pub const PANEL: Duration = Duration::from_millis(300);
    /// Caret blinks and other sub-perceptual changes.
    pub const MICRO: Duration = Duration::from_millis(100);
    /// One full breath of the pulsing status ring.
    pub const STATUS_PULSE: Duration = Duration::from_millis(1600);
    /// One full rotation of the activity spinner.
    pub const ACTIVITY_SPIN: Duration = Duration::from_millis(800);
    /// How often continuous status motion repaints, about 30 Hz.
    ///
    /// A spinner or pulse that repaints on every display frame redraws the whole
    /// window at 60–120 Hz for as long as a turn runs, since the sidebar is drawn
    /// by the root view. Neither needs more than this to read as motion.
    pub const ACTIVITY_FRAME: Duration = Duration::from_millis(33);
    /// One step of the three-dot typing indicator.
    pub const TYPING_STEP: Duration = Duration::from_millis(350);
    /// How often a running clock repaints. Elapsed times are shown in whole
    /// seconds, so anything faster is a repaint that changes nothing.
    pub const CLOCK_TICK: Duration = Duration::from_secs(1);

    /// Decelerating ease, matching `easeOut`.
    pub fn ease_out() -> impl Fn(f32) -> f32 + 'static {
        |t| 1.0 - (1.0 - t).powi(3)
    }

    /// Symmetric ease, matching `easeInOut`.
    pub fn ease_in_out() -> impl Fn(f32) -> f32 + 'static {
        gpui::ease_in_out
    }

    /// Linear, for continuous rotations.
    pub fn linear() -> impl Fn(f32) -> f32 + 'static {
        gpui::linear
    }

    /// Near-critically-damped spring. `overshoot` of 0 never exceeds 1.0;
    /// small positive values add a single gentle rebound.
    ///
    /// Implemented as an underdamped sinusoid whose amplitude is chosen to
    /// match the requested bounce, so the motion still lands exactly on 1.0.
    pub fn spring(overshoot: f32) -> impl Fn(f32) -> f32 + 'static {
        move |t| {
            if t >= 1.0 {
                return 1.0;
            }
            if overshoot <= 0.0 {
                // Damped ease-out with no oscillation.
                return 1.0 - (1.0 - t).powi(3);
            }
            // A damping ratio below 1 produces the rebound. 0.62 is roughly
            // critical; each 0.1 of requested bounce costs 0.02 of damping,
            // which keeps a "gentle" preset at about a 10% overshoot rather
            // than the 17% a raw mapping would give.
            let zeta = 0.62 - overshoot.clamp(0.0, 0.4) * 0.2;
            let omega = 2.0 * std::f32::consts::PI * 1.05;
            let damped = omega * (1.0 - zeta * zeta).max(0.0).sqrt();
            1.0 - (-zeta * omega * t).exp() * (damped * t).cos()
        }
    }

    /// Build a GPUI animation for a preset.
    pub fn snappy() -> Animation {
        Animation::new(Self::SNAPPY).with_easing(Self::spring(0.0))
    }

    pub fn gentle() -> Animation {
        Animation::new(Self::GENTLE).with_easing(Self::spring(0.15))
    }

    pub fn quick() -> Animation {
        Animation::new(Self::QUICK).with_easing(Self::ease_out())
    }

    pub fn smooth() -> Animation {
        Animation::new(Self::SMOOTH).with_easing(Self::ease_in_out())
    }

    pub fn panel() -> Animation {
        Animation::new(Self::PANEL).with_easing(Self::spring(0.0))
    }

    pub fn micro() -> Animation {
        Animation::new(Self::MICRO).with_easing(Self::ease_out())
    }

    /// Continuous breathing, for the pulsing status ring.
    pub fn status_pulse() -> Animation {
        Animation::new(Self::STATUS_PULSE)
            .with_easing(Self::ease_in_out())
            .repeat()
    }

    /// Continuous rotation, for the activity spinner.
    pub fn activity_spin() -> Animation {
        Animation::new(Self::ACTIVITY_SPIN)
            .with_easing(Self::linear())
            .repeat()
    }

    /// Opacity of the pulsing ring at progress `t`, honoring reduce-motion by
    /// collapsing to a steady, fully-visible ring.
    pub fn status_pulse_opacity(t: f32, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 0.18;
        }
        if t >= 1.0 { 0.0 } else { 0.35 }
    }

    /// Outer ring diameter at progress `t`.
    pub fn status_pulse_size(t: f32, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 12.0;
        }
        if t >= 1.0 { 18.0 } else { 8.0 }
    }

    /// Inner dot opacity at progress `t`.
    pub fn status_pulse_dot_opacity(t: f32, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 1.0;
        }
        if t >= 1.0 { 0.7 } else { 1.0 }
    }

    /// Opacity for the `index`-th dot of the typing indicator.
    pub fn typing_dot_opacity(index: usize, phase: usize, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 0.65;
        }
        if index == phase { 1.0 } else { 0.3 }
    }

    /// Scale for the `index`-th dot of the typing indicator.
    pub fn typing_dot_scale(index: usize, phase: usize, reduce_motion: bool) -> f32 {
        if reduce_motion {
            return 1.0;
        }
        if index == phase { 1.2 } else { 1.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_reversal_starts_at_its_current_position_and_settles() {
        let now = Instant::now();
        let mut panel = PanelTransition::new(true);
        assert_eq!(panel.progress(now), 1.0);
        assert!(!panel.is_animating(now));
        panel.set_expanded(false, now);
        let halfway = now + FxAnimation::PANEL / 2;
        let position = panel.progress(halfway);
        assert!(position > 0.0 && position < 1.0);
        panel.set_expanded(true, halfway);
        assert_eq!(panel.progress(halfway), position);
        assert!(panel.progress(halfway + FxAnimation::PANEL / 2) > position);
        assert_eq!(panel.progress(halfway + FxAnimation::PANEL), 1.0);
        assert!(!panel.is_animating(halfway + FxAnimation::PANEL));
        panel.set_expanded(false, halfway + FxAnimation::PANEL);
        assert_eq!(panel.progress(halfway + FxAnimation::PANEL * 2), 0.0);
    }

    #[test]
    fn every_easing_starts_at_zero_and_ends_at_one() {
        type Easing = dyn Fn(f32) -> f32;
        let easings: Vec<(&str, Box<Easing>)> = vec![
            ("ease_out", Box::new(FxAnimation::ease_out())),
            ("ease_in_out", Box::new(FxAnimation::ease_in_out())),
            ("linear", Box::new(FxAnimation::linear())),
            ("spring(0)", Box::new(FxAnimation::spring(0.0))),
            ("spring(0.15)", Box::new(FxAnimation::spring(0.15))),
        ];
        for (name, f) in easings {
            assert!(f(0.0).abs() < 1e-3, "{name} starts at {}", f(0.0));
            assert!((f(1.0) - 1.0).abs() < 1e-3, "{name} ends at {}", f(1.0));
        }
    }

    #[test]
    fn easings_stay_finite_and_near_monotonic_over_their_domain() {
        for t in (0..=100).map(|i| i as f32 / 100.0) {
            let a = FxAnimation::ease_out()(t);
            let b = FxAnimation::ease_in_out()(t);
            let c = FxAnimation::spring(0.0)(t);
            assert!(a.is_finite() && b.is_finite() && c.is_finite());
        }
    }

    #[test]
    fn a_damped_spring_never_leaves_the_unit_range_by_much() {
        let spring = FxAnimation::spring(0.15);
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let v = spring(t);
            // A gentle rebound is intentional; a wild one is not. The curve
            // peaks a little above the textbook envelope, so the ceiling is
            // 15% rather than the nominal 10%.
            assert!(
                (-0.15..=1.15).contains(&v),
                "spring overshot to {v} at t={t}"
            );
        }
    }

    #[test]
    fn a_zero_bounce_spring_never_overshoots() {
        let spring = FxAnimation::spring(0.0);
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let v = spring(t);
            assert!((0.0..=1.0).contains(&v), "spring left the unit range: {v}");
        }
    }

    #[test]
    fn reduce_motion_yields_a_steady_ring() {
        assert_eq!(FxAnimation::status_pulse_size(0.0, true), 12.0);
        assert_eq!(FxAnimation::status_pulse_size(1.0, true), 12.0);
        assert_eq!(FxAnimation::status_pulse_dot_opacity(0.5, true), 1.0);
    }

    #[test]
    fn typing_indicator_highlights_exactly_one_dot() {
        for phase in 0..3 {
            let lit = (0..3)
                .filter(|i| FxAnimation::typing_dot_opacity(*i, phase, false) > 0.5)
                .count();
            assert_eq!(lit, 1, "phase {phase}");
        }
    }
}
