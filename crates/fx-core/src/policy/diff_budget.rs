//! Bounds the total text assembled for one project's diff.
//!
//! A diff is collected from several sources — one tracked patch plus one patch
//! per untracked file — so no single command can be trusted to stay inside a
//! limit. This budget spans the whole assembly: it hands each source whatever
//! room is left, and stops collecting once that room runs out.
//!
//! Truncation is announced rather than silent. A user reading a half-patch
//! needs to know the other half exists.

/// Default ceiling on the assembled diff.
pub const DEFAULT_MAXIMUM_BYTES: usize = 16 * 1024 * 1024;

/// Appended when the budget is exceeded.
pub const TRUNCATION_NOTICE: &str = "[FlowX truncated this project diff because it exceeded the safe display limit. Remaining changes are not shown.]";

/// The smallest ceiling that can hold the notice plus its separator.
///
/// Below this, the notice itself is the largest thing the output can be, since
/// announcing a truncation is never optional.
pub const MINIMUM_MAXIMUM_BYTES: usize = TRUNCATION_NOTICE.len() + SEPARATOR.len();

/// The separator between independently collected patches.
const SEPARATOR: &str = "\n\n";

/// Tracks how much room is left in the assembled diff.
#[derive(Debug, Clone)]
pub struct DiffBudget {
    maximum_bytes: usize,
    content_byte_limit: usize,
    chunks: Vec<String>,
    content_byte_count: usize,
    was_truncated: bool,
}

impl Default for DiffBudget {
    fn default() -> Self {
        Self::new(DEFAULT_MAXIMUM_BYTES)
    }
}

impl DiffBudget {
    /// Create a budget with a ceiling, reserving room for the truncation notice
    /// up front so the notice can never itself overflow the limit.
    ///
    /// A ceiling below [`MINIMUM_MAXIMUM_BYTES`] is honored as far as possible:
    /// no content is kept, and the output is the notice alone.
    pub fn new(maximum_bytes: usize) -> Self {
        let reserved = TRUNCATION_NOTICE.len() + SEPARATOR.len();
        let content_byte_limit = maximum_bytes.saturating_sub(reserved);
        Self {
            maximum_bytes,
            content_byte_limit,
            chunks: Vec::new(),
            content_byte_count: 0,
            was_truncated: false,
        }
    }

    /// Whether any source was cut short.
    pub fn was_truncated(&self) -> bool {
        self.was_truncated
    }

    /// The content ceiling, excluding room reserved for the notice.
    pub fn content_byte_limit(&self) -> usize {
        self.content_byte_limit
    }

    /// Bytes of content accepted so far, excluding the notice.
    pub fn content_bytes(&self) -> usize {
        self.content_byte_count
    }

    /// How many bytes the next source may return, including the separator it
    /// will be preceded by.
    pub fn remaining_fragment_bytes(&self) -> usize {
        let separator = if self.chunks.is_empty() {
            0
        } else {
            SEPARATOR.len()
        };
        self.content_byte_limit
            .saturating_sub(self.content_byte_count + separator)
    }

    /// Append one patch. Returns `false` once no further source should be
    /// collected.
    ///
    /// `source_was_truncated` says the *command itself* hit its own output
    /// ceiling, which ends collection independently of this budget.
    pub fn append(&mut self, fragment: &str, source_was_truncated: bool) -> bool {
        let separator = if self.chunks.is_empty() {
            String::new()
        } else {
            SEPARATOR.to_string()
        };
        let bounded = utf8_prefix(fragment, self.remaining_fragment_bytes());

        if !bounded.is_empty() {
            self.chunks.push(bounded.clone());
            self.content_byte_count += separator.len() + bounded.len();
        }

        // Either the source was already cut, or we cut it.
        if source_was_truncated || bounded.len() < fragment.len() {
            self.was_truncated = true;
            return false;
        }

        self.remaining_fragment_bytes() > 0
    }

    /// Force the truncated flag, for a failure partway through collection.
    pub fn mark_truncated(&mut self) {
        self.was_truncated = true;
    }

    /// The assembled diff, with the notice appended when truncated.
    ///
    /// The result is at most `max(maximum_bytes, MINIMUM_MAXIMUM_BYTES)`: a
    /// ceiling too small to hold the notice still yields the notice, because a
    /// silently truncated diff is worse than a slightly oversized message.
    pub fn output(&self) -> String {
        let mut result = self.chunks.join(SEPARATOR);
        if self.was_truncated {
            if !result.is_empty() {
                result.push_str(SEPARATOR);
            }
            result.push_str(TRUNCATION_NOTICE);
        }
        debug_assert!(
            result.len() <= self.maximum_bytes.max(MINIMUM_MAXIMUM_BYTES),
            "output was {} bytes against a {} byte budget",
            result.len(),
            self.maximum_bytes
        );
        result
    }
}

/// The longest prefix of `text` that fits in `maximum_bytes` bytes without
/// splitting a character.
///
/// Cutting mid-character would produce invalid UTF-8, so the budget is spent
/// only on whole characters.
fn utf8_prefix(text: &str, maximum_bytes: usize) -> String {
    if maximum_bytes == 0 {
        return String::new();
    }
    if text.len() <= maximum_bytes {
        return text.to_string();
    }
    let mut end = maximum_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_are_joined_with_a_blank_line() {
        let mut b = DiffBudget::new(1_000_000);
        assert!(b.append("diff --git a/one", false));
        assert!(b.append("diff --git a/two", false));
        assert_eq!(b.output(), "diff --git a/one\n\ndiff --git a/two");
    }

    #[test]
    fn a_single_small_diff_passes_through_unchanged() {
        let mut b = DiffBudget::new(1_000_000);
        b.append("hello", false);
        assert_eq!(b.output(), "hello");
        assert!(!b.was_truncated());
    }

    #[test]
    fn exceeding_the_budget_truncates_and_announces() {
        let ceiling = MINIMUM_MAXIMUM_BYTES + 200;
        let mut b = DiffBudget::new(ceiling);
        let big = "x".repeat(10_000);
        assert!(!b.append(&big, false), "collection must stop");
        assert!(b.was_truncated());
        let out = b.output();
        assert!(out.ends_with(TRUNCATION_NOTICE));
        assert!(out.len() <= ceiling, "output was {} bytes", out.len());
    }

    #[test]
    fn the_notice_is_the_only_thing_shown_when_nothing_fits() {
        let mut b = DiffBudget::new(50);
        b.mark_truncated();
        assert_eq!(b.output(), TRUNCATION_NOTICE);
        assert_eq!(b.content_bytes(), 0);
    }

    #[test]
    fn a_ceiling_too_small_for_the_notice_still_announces_truncation() {
        // Below MINIMUM_MAXIMUM_BYTES there is no way to both respect the
        // ceiling and tell the user anything. Announcing wins.
        let mut b = DiffBudget::new(10);
        b.append(&"z".repeat(100), false);
        assert_eq!(b.output(), TRUNCATION_NOTICE);
        assert!(b.output().len() <= MINIMUM_MAXIMUM_BYTES);
    }

    #[test]
    fn a_truncated_source_stops_collection_regardless_of_room() {
        let mut b = DiffBudget::new(1_000_000);
        assert!(!b.append("small", true), "the command itself was cut");
        assert!(b.was_truncated());
        assert!(b.output().contains(TRUNCATION_NOTICE));
    }

    #[test]
    fn the_remaining_room_shrinks_as_fragments_arrive() {
        let mut b = DiffBudget::new(1000);
        let first = b.remaining_fragment_bytes();
        assert!(b.append(&"a".repeat(100), false));
        let second = b.remaining_fragment_bytes();
        assert!(second < first, "{second} should be under {first}");
        // The separator is charged to the next fragment.
        assert_eq!(second, first - 100 - SEPARATOR.len());
    }

    #[test]
    fn the_first_fragment_is_charged_no_separator() {
        let mut b = DiffBudget::new(1000);
        assert!(b.append(&"a".repeat(100), false));
        // No separator was charged for the first append. The remaining figure
        // is one separator short because the *next* append will need one.
        assert_eq!(b.content_bytes(), 100);
        assert_eq!(
            b.remaining_fragment_bytes(),
            b.content_byte_limit() - 100 - SEPARATOR.len()
        );
    }

    #[test]
    fn a_second_fragment_is_charged_its_separator() {
        let mut b = DiffBudget::new(1000);
        b.append(&"a".repeat(100), false);
        assert_eq!(b.content_bytes(), 100);
        b.append(&"b".repeat(100), false);
        assert_eq!(b.content_bytes(), 100 + SEPARATOR.len() + 100);
    }

    #[test]
    fn a_fragment_larger_than_the_budget_is_cut_without_overflowing() {
        let ceiling = MINIMUM_MAXIMUM_BYTES + 120;
        let mut b = DiffBudget::new(ceiling);
        b.append(&"y".repeat(50_000), false);
        let out = b.output();
        assert!(out.len() <= ceiling, "{} bytes", out.len());
        assert!(out.ends_with(TRUNCATION_NOTICE));
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        // Each 'é' is two bytes, so an odd content limit must round down to
        // keep them whole.
        let ceiling = MINIMUM_MAXIMUM_BYTES + 103;
        let mut b = DiffBudget::new(ceiling);
        b.append(&"é".repeat(1000), false);
        let out = b.output();
        assert!(out.len() <= ceiling, "{} bytes", out.len());
        // A split character would decode as replacement characters.
        assert!(!out.contains('\u{FFFD}'));
    }

    #[test]
    fn a_budget_just_large_enough_for_the_notice_keeps_no_content() {
        let mut b = DiffBudget::new(MINIMUM_MAXIMUM_BYTES);
        assert_eq!(b.content_byte_limit(), 0);
        b.append(&"z".repeat(100), false);
        assert_eq!(b.output(), TRUNCATION_NOTICE);
    }

    #[test]
    fn the_utf8_prefix_helper_leaves_short_text_alone() {
        assert_eq!(utf8_prefix("abc", 10), "abc");
        assert_eq!(utf8_prefix("abc", 3), "abc");
        assert_eq!(utf8_prefix("abc", 2), "ab");
        assert_eq!(utf8_prefix("abc", 0), "");
    }
}
