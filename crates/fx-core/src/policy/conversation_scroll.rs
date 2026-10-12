//! Scroll geometry for the conversation surface.
//!
//! Keeping this math out of the view layer is what makes the restore and
//! pinned-to-bottom contracts testable without building a window.

/// A resolved scroll position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollMetrics {
    pub offset: f32,
    pub max_offset: f32,
}

pub struct ConversationScrollPolicy;

impl ConversationScrollPolicy {
    /// How close to the bottom still counts as pinned.
    ///
    /// Without a tolerance, subpixel layout differences would unpin the view
    /// constantly and the transcript would stop following a streaming reply.
    pub const BOTTOM_TOLERANCE: f32 = 24.0;

    /// Resolve a raw scroll position against the content's geometry.
    pub fn metrics(
        content_offset_y: f32,
        content_height: f32,
        top_inset: f32,
        bottom_inset: f32,
        container_height: f32,
    ) -> ScrollMetrics {
        let max_offset = (content_height + top_inset + bottom_inset - container_height).max(0.0);
        let offset = (content_offset_y + top_inset).clamp(0.0, max_offset);
        ScrollMetrics { offset, max_offset }
    }

    /// Whether the view should keep following new content.
    ///
    /// Content shorter than its container is pinned by definition: there is
    /// nowhere else to be.
    pub fn is_pinned_to_bottom(metrics: ScrollMetrics, tolerance: f32) -> bool {
        metrics.max_offset <= 1.0 || metrics.offset >= metrics.max_offset - tolerance.max(0.0)
    }

    /// Whether the view is pinned, using the default tolerance.
    pub fn is_pinned(metrics: ScrollMetrics) -> bool {
        Self::is_pinned_to_bottom(metrics, Self::BOTTOM_TOLERANCE)
    }
}

/// Decides when the transcript has settled enough to be revealed.
///
/// The alternative — imposing a fixed delay before showing a transcript — makes
/// every thread switch feel sluggish. This instead waits for the content
/// geometry to stop moving.
///
/// The first observation only records a baseline; convergence is reported once
/// three *further* passes leave `max_offset` unchanged. So a document that is
/// already stable settles on the fourth call.
#[derive(Debug, Clone, Default)]
pub struct ConversationScrollRestoration {
    previous_max_offset: Option<f32>,
    stable_passes: u32,
}

/// How many consecutive unchanged passes count as settled.
const REQUIRED_STABLE_PASSES: u32 = 3;

/// Sub-pixel slack, below which two measurements count as equal.
const STABLE_EPSILON: f32 = 0.5;

impl ConversationScrollRestoration {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one layout pass. Returns `true` once the geometry has settled
    /// and the transcript can be shown.
    pub fn observe(&mut self, max_offset: f32, desired_offset: f32, stick_to_bottom: bool) -> bool {
        // Only converge on a target we could actually reach. A remembered
        // offset from a taller document must not block revealing a shorter one.
        let target_available = stick_to_bottom || desired_offset <= max_offset + STABLE_EPSILON;

        self.stable_passes = if target_available
            && self
                .previous_max_offset
                .is_some_and(|previous| (previous - max_offset).abs() <= STABLE_EPSILON)
        {
            self.stable_passes + 1
        } else {
            0
        };

        self.previous_max_offset = Some(max_offset);
        self.stable_passes >= REQUIRED_STABLE_PASSES
    }

    /// Forget the settled state, so the next reveal waits again.
    pub fn reset(&mut self) {
        self.previous_max_offset = None;
        self.stable_passes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_shorter_than_its_container_cannot_scroll() {
        let m = ConversationScrollPolicy::metrics(0.0, 100.0, 0.0, 0.0, 500.0);
        assert_eq!(m.max_offset, 0.0);
        assert_eq!(m.offset, 0.0);
        assert!(ConversationScrollPolicy::is_pinned(m));
    }

    #[test]
    fn the_offset_includes_the_top_inset_and_clamps_to_the_range() {
        let m = ConversationScrollPolicy::metrics(50.0, 1000.0, 20.0, 0.0, 400.0);
        assert_eq!(m.max_offset, 620.0);
        assert_eq!(m.offset, 70.0, "the top inset shifts the content down");
    }

    #[test]
    fn a_negative_offset_is_clamped_to_the_top() {
        let m = ConversationScrollPolicy::metrics(-500.0, 1000.0, 0.0, 0.0, 400.0);
        assert_eq!(m.offset, 0.0);
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_to_the_bottom() {
        let m = ConversationScrollPolicy::metrics(9999.0, 1000.0, 0.0, 0.0, 400.0);
        assert_eq!(m.offset, 600.0);
        assert_eq!(m.offset, m.max_offset);
    }

    #[test]
    fn the_bottom_inset_extends_the_scrollable_range() {
        let without = ConversationScrollPolicy::metrics(0.0, 1000.0, 0.0, 0.0, 400.0);
        let with = ConversationScrollPolicy::metrics(0.0, 1000.0, 0.0, 200.0, 400.0);
        assert_eq!(with.max_offset, without.max_offset + 200.0);
    }

    #[test]
    fn being_within_tolerance_of_the_bottom_counts_as_pinned() {
        let m = ScrollMetrics {
            offset: 600.0,
            max_offset: 610.0,
        };
        assert!(
            ConversationScrollPolicy::is_pinned(m),
            "10pt from the bottom"
        );
    }

    #[test]
    fn being_well_above_the_bottom_is_not_pinned() {
        let m = ScrollMetrics {
            offset: 400.0,
            max_offset: 1000.0,
        };
        assert!(!ConversationScrollPolicy::is_pinned(m));
    }

    #[test]
    fn restoration_reports_settled_after_three_unchanged_passes() {
        let mut r = ConversationScrollRestoration::new();
        // The first call records a baseline; three more must agree with it.
        assert!(!r.observe(600.0, 600.0, false), "baseline");
        assert!(!r.observe(600.0, 600.0, false), "pass 1");
        assert!(!r.observe(600.0, 600.0, false), "pass 2");
        assert!(r.observe(600.0, 600.0, false), "pass 3");
    }

    #[test]
    fn a_changing_geometry_restarts_the_count() {
        let mut r = ConversationScrollRestoration::new();
        r.observe(600.0, 600.0, false);
        r.observe(600.0, 600.0, false);
        // Content grew while the view was being prepared.
        assert!(!r.observe(1200.0, 600.0, false), "geometry changed");
        assert!(!r.observe(1200.0, 600.0, false));
        assert!(!r.observe(1200.0, 600.0, false));
        assert!(r.observe(1200.0, 600.0, false));
    }

    #[test]
    fn subpixel_change_is_treated_as_unchanged() {
        let mut r = ConversationScrollRestoration::new();
        r.observe(600.0, 600.0, false);
        r.observe(600.4, 600.0, false);
        r.observe(600.2, 600.0, false);
        assert!(
            r.observe(600.6, 600.0, false),
            "0.6pt of drift is still stable"
        );
    }

    #[test]
    fn a_large_geometry_jump_does_not_count_as_stable() {
        let mut r = ConversationScrollRestoration::new();
        r.observe(600.0, 600.0, false);
        r.observe(600.0, 600.0, false);
        r.observe(900.0, 600.0, false);
        assert!(!r.observe(900.0, 600.0, false), "the count restarted");
    }

    #[test]
    fn an_unreachable_target_never_converges() {
        // A remembered offset past the end of a shorter document must not hold
        // the transcript hostage.
        let mut r = ConversationScrollRestoration::new();
        for _ in 0..10 {
            assert!(
                !r.observe(100.0, 5000.0, false),
                "a target beyond max_offset should never settle"
            );
        }
    }

    #[test]
    fn sticking_to_the_bottom_converges_regardless_of_the_target() {
        let mut r = ConversationScrollRestoration::new();
        assert!(!r.observe(100.0, 5000.0, true), "baseline");
        assert!(!r.observe(100.0, 5000.0, true));
        assert!(!r.observe(100.0, 5000.0, true));
        assert!(r.observe(100.0, 5000.0, true));
    }

    #[test]
    fn resetting_forgets_the_settled_state() {
        let mut r = ConversationScrollRestoration::new();
        for _ in 0..4 {
            r.observe(600.0, 600.0, false);
        }
        assert!(r.observe(600.0, 600.0, false));

        r.reset();
        assert!(!r.observe(600.0, 600.0, false), "the count restarted");
    }
}
