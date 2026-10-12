//! Disclosure state for the diff list.
//!
//! Bulk disclosure is a *default plus per-file exceptions*, not a stored flag
//! per file. That is what makes Collapse All apply to files that arrive on a
//! later refresh, and what keeps opening one file from expanding the rest.

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffDisclosureState {
    default_collapsed: bool,
    /// Files that disagree with the default.
    exceptions: BTreeSet<String>,
}

impl DiffDisclosureState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a file's diff is hidden behind its header.
    pub fn is_collapsed(&self, id: &str) -> bool {
        self.default_collapsed != self.exceptions.contains(id)
    }

    /// Pin one file to a state. Pinning it back to the default drops the
    /// exception rather than storing a redundant one, so the set only ever
    /// holds genuine disagreements.
    pub fn set_collapsed(&mut self, collapsed: bool, id: &str) {
        if collapsed == self.default_collapsed {
            self.exceptions.remove(id);
        } else {
            self.exceptions.insert(id.to_string());
        }
    }

    pub fn toggle(&mut self, id: &str) {
        self.set_collapsed(!self.is_collapsed(id), id);
    }

    /// Collapse or expand everything, including files not yet discovered.
    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        self.default_collapsed = collapsed;
        self.exceptions.clear();
    }

    pub fn is_all_collapsed(&self) -> bool {
        self.default_collapsed
    }

    /// The files currently disagreeing with the default.
    pub fn exceptions(&self) -> impl Iterator<Item = &str> {
        self.exceptions.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_collapsed_by_default() {
        let s = DiffDisclosureState::new();
        assert!(!s.is_collapsed("a"));
        assert!(!s.is_collapsed("b"));
        assert!(!s.is_all_collapsed());
    }

    #[test]
    fn collapsing_one_file_leaves_the_others_alone() {
        let mut s = DiffDisclosureState::new();
        s.set_collapsed(true, "a");
        assert!(s.is_collapsed("a"));
        assert!(!s.is_collapsed("b"));
    }

    #[test]
    fn collapsing_all_applies_to_files_discovered_later() {
        let mut s = DiffDisclosureState::new();
        s.set_all_collapsed(true);
        // "later" was never mentioned; it still honours the default.
        assert!(s.is_collapsed("later"));
        assert!(s.is_collapsed("a"));
        assert_eq!(s.exceptions().count(), 0, "the default needs no exceptions");
    }

    #[test]
    fn expand_all_clears_a_previous_exception() {
        let mut s = DiffDisclosureState::new();
        s.set_collapsed(true, "a");
        assert_eq!(s.exceptions().count(), 1);

        s.set_all_collapsed(true);
        // Setting the default to the same value the exception already matches
        // would leave `a` as an exception against a collapsed default.
        s.set_all_collapsed(true);
        assert!(s.is_collapsed("a"));

        s.set_all_collapsed(false);
        assert!(!s.is_collapsed("a"));
        assert!(!s.is_collapsed("b"));
    }

    #[test]
    fn expand_all_clears_per_file_overrides() {
        let mut s = DiffDisclosureState::new();
        s.set_collapsed(true, "a");
        s.set_collapsed(true, "b");
        s.set_all_collapsed(true);
        assert!(s.is_collapsed("a"));
        assert!(s.is_collapsed("b"));

        s.set_all_collapsed(false);
        assert_eq!(s.exceptions().count(), 0);
        assert!(!s.is_collapsed("a"));
    }

    #[test]
    fn pinning_back_to_the_default_drops_the_exception() {
        let mut s = DiffDisclosureState::new();
        s.set_collapsed(true, "a");
        assert_eq!(s.exceptions().count(), 1);
        s.set_collapsed(false, "a");
        assert_eq!(s.exceptions().count(), 0, "the exception was redundant");
    }

    #[test]
    fn an_exception_is_measured_against_the_current_default() {
        let mut s = DiffDisclosureState::new();
        s.set_collapsed(true, "a");
        assert!(s.is_collapsed("a"));
        assert_eq!(s.exceptions().count(), 1);

        // Flip the default without going through `set_all_collapsed`, which
        // would clear exceptions. `a` was recorded as an exception *because* it
        // disagreed with an expanded default; now that both defaults are
        // collapsed it agrees again, so it reads as expanded.
        s.default_collapsed = true;
        assert_eq!(s.exceptions().count(), 1, "the exception is still stored");
        assert!(
            !s.is_collapsed("a"),
            "a now agrees with the collapsed default, so it reads as expanded"
        );
        assert!(
            s.is_collapsed("b"),
            "the default did collapse everything else"
        );
    }

    #[test]
    fn toggling_flips_only_the_named_file() {
        let mut s = DiffDisclosureState::new();
        s.toggle("a");
        assert!(s.is_collapsed("a"));
        s.toggle("a");
        assert!(!s.is_collapsed("a"));
        assert!(!s.is_collapsed("b"));
    }
}
