//! Coalescing a provider's text stream into UI updates.
//!
//! A provider emits deltas as fast as it can decode them, which is far faster
//! than a transcript can be laid out. The original coalesced them to a 50 ms
//! cadence while publishing the first delta immediately, so a reply starts
//! appearing right away and then settles into a steady rhythm.
//!
//! Two details matter and are easy to get wrong:
//!
//! - The **first** delta is published at once. Waiting even one cadence before
//!   showing anything makes a fast provider feel broken.
//! - The wait uses a **trailing** timer. A provider that pauses mid-sentence
//!   still gets its text delivered rather than sitting in the buffer forever.

use std::time::{Duration, Instant};

/// How long to wait between publications after the first.
pub const DEFAULT_CADENCE: Duration = Duration::from_millis(50);

/// What the buffer decided to do with a burst of deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cadence {
    /// Publish now.
    Immediate,
    /// Wait until the next tick.
    Deferred,
}

/// The decision half of the buffer, separated from the timer so it can be tested
/// without waiting on real time.
#[derive(Debug, Clone)]
pub struct DeltaPolicy {
    published_anything: bool,
    pending: String,
    last_flush: Option<Instant>,
    cadence: Duration,
}

impl Default for DeltaPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_CADENCE)
    }
}

impl DeltaPolicy {
    pub fn new(cadence: Duration) -> Self {
        Self {
            published_anything: false,
            pending: String::new(),
            last_flush: None,
            cadence,
        }
    }

    /// Record a delta and report whether it should be published now.
    pub fn accept(&mut self, delta: &str, now: Instant) -> Cadence {
        self.pending.push_str(delta);

        // The very first text goes out immediately, whatever the cadence.
        if !self.published_anything {
            self.published_anything = true;
            self.last_flush = Some(now);
            return Cadence::Immediate;
        }

        match self.last_flush {
            Some(last) if now.duration_since(last) >= self.cadence => {
                self.last_flush = Some(now);
                Cadence::Immediate
            }
            _ => Cadence::Deferred,
        }
    }

    /// Take the buffered text, if any, and note that it was published.
    pub fn flush(&mut self, now: Instant) -> Option<String> {
        self.last_flush = Some(now);
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    /// Throw away buffered text.
    ///
    /// Used when a turn is cancelled or replaced: partial text from a dead turn
    /// must not appear in the next one.
    pub fn discard(&mut self) {
        self.pending.clear();
    }

    pub fn pending(&self) -> &str {
        &self.pending
    }

    /// Whether anything has been published yet this turn.
    pub fn has_published(&self) -> bool {
        self.published_anything
    }

    /// Whether text is waiting to be shown.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the cadence has elapsed and a flush is due.
    pub fn is_due(&self, now: Instant) -> bool {
        self.has_pending()
            && self
                .last_flush
                .is_none_or(|last| now.duration_since(last) >= self.cadence)
    }

    /// Start over for a new turn.
    pub fn reset(&mut self) {
        self.published_anything = false;
        self.pending.clear();
        self.last_flush = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn the_first_delta_is_published_immediately() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        assert_eq!(p.accept("hello", now), Cadence::Immediate);
    }

    #[test]
    fn deltas_inside_the_cadence_are_deferred() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        assert_eq!(p.accept("a", now), Cadence::Immediate);
        assert_eq!(p.accept("b", at(now, 10)), Cadence::Deferred);
        assert_eq!(p.accept("c", at(now, 20)), Cadence::Deferred);
    }

    #[test]
    fn a_delta_past_the_cadence_publishes_immediately() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("a", now);
        assert_eq!(p.accept("b", at(now, 60)), Cadence::Immediate);
    }

    #[test]
    fn deferred_deltas_are_still_delivered() {
        // A provider that pauses must not lose its text.
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("one", now);
        p.accept(" two", at(now, 10));
        p.accept(" three", at(now, 20));
        assert!(p.has_pending());
        assert_eq!(
            p.flush(at(now, 20)).as_deref(),
            Some("one two three"),
            "buffered text was lost"
        );
    }

    #[test]
    fn flushing_takes_everything_accumulated_so_far() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("hello", now);
        p.accept(" world", at(now, 5));
        assert_eq!(p.flush(at(now, 5)).as_deref(), Some("hello world"));
        assert!(!p.has_pending());
    }

    #[test]
    fn flushing_nothing_reports_nothing() {
        let mut p = DeltaPolicy::default();
        assert_eq!(p.flush(Instant::now()), None);
    }

    #[test]
    fn flushing_twice_does_not_repeat_the_text() {
        // The bug this prevents: a timer firing after a manual flush would
        // publish the same characters twice.
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("x", now);
        assert_eq!(p.flush(now).as_deref(), Some("x"));
        assert_eq!(p.flush(now), None);
    }

    #[test]
    fn discarding_drops_buffered_text() {
        // Cancelling a turn must not leak partial text into the next one.
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("half a sen", now);
        p.discard();
        assert!(!p.has_pending());
        assert_eq!(p.flush(now), None);
    }

    #[test]
    fn resetting_makes_the_next_delta_immediate_again() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("first", now);
        p.accept("deferred", at(now, 1));
        p.reset();

        assert!(!p.has_published());
        assert!(!p.has_pending());
        let later = at(now, 5);
        assert_eq!(
            p.accept("new turn", later),
            Cadence::Immediate,
            "a new turn must not wait out the old turn's cadence"
        );
    }

    #[test]
    fn due_is_false_before_the_cadence_and_true_after() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("a", now);
        p.accept("b", at(now, 10));
        assert!(!p.is_due(at(now, 20)));
        assert!(p.is_due(at(now, 60)));
    }

    #[test]
    fn due_is_false_with_nothing_pending() {
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("a", now);
        p.flush(now);
        assert!(
            !p.is_due(at(now, 10_000)),
            "a timer fired with nothing to show"
        );
    }

    #[test]
    fn a_due_check_does_not_consume_the_buffer() {
        // The timer asks whether to flush; the flush itself takes the text.
        let mut p = DeltaPolicy::default();
        let now = Instant::now();
        p.accept("a", now);
        p.accept("b", at(now, 10));
        assert!(p.is_due(at(now, 60)));
        assert!(p.is_due(at(now, 70)));
        assert!(p.has_pending());
    }

    #[test]
    fn a_custom_cadence_is_honored() {
        let mut p = DeltaPolicy::new(Duration::from_millis(500));
        let now = Instant::now();
        p.accept("a", now);
        assert_eq!(p.accept("b", at(now, 100)), Cadence::Deferred);
        assert_eq!(p.accept("c", at(now, 600)), Cadence::Immediate);
    }

    #[test]
    fn the_default_cadence_matches_the_documented_value() {
        assert_eq!(DEFAULT_CADENCE, Duration::from_millis(50));
    }
}
