//! Row indexing for the virtualized diff list.
//!
//! The diff view shows tens of thousands of code rows. Building an element per
//! row would make memory and rebuild cost scale with line count, which is
//! exactly what the original design set out to avoid: **cost depends on the
//! number of files, not the number of lines.**
//!
//! So the layout stores only a prefix sum of row counts — one integer per file
//! — and answers "which file and line is row N?" with a binary search. No diff
//! text is copied, measured, or retained here.

/// A position in the flattened diff list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// Index into the caller's file list.
    pub section: usize,
    /// Line within that file. `None` identifies the section's header row.
    pub line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRowLayout {
    /// `starts[i]` is the first row of section `i`.
    starts: Vec<usize>,
    count: usize,
}

impl DiffRowLayout {
    /// Index the list.
    ///
    /// `line_counts[i]` is how many code rows file `i` contributes when
    /// expanded; a collapsed file contributes only its header.
    pub fn new(line_counts: &[usize], collapsed_sections: &dyn Fn(usize) -> bool) -> Self {
        let mut starts = Vec::with_capacity(line_counts.len());
        let mut count = 0usize;
        for (section, lines) in line_counts.iter().enumerate() {
            starts.push(count);
            // Every section always has its header row; a collapsed one has
            // nothing else.
            count += 1 + if collapsed_sections(section) {
                0
            } else {
                *lines
            };
        }
        Self { starts, count }
    }

    /// Total number of visible rows, headers included.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The first row of a section, or `None` when the section does not exist.
    pub fn header_row(&self, section: usize) -> Option<usize> {
        self.starts.get(section).copied()
    }

    /// Resolve a row index to a file and line.
    ///
    /// Returns `None` outside the visible range, which is the normal answer for
    /// a scroll position between rows.
    pub fn location(&self, row: usize) -> Option<Location> {
        if row >= self.count {
            return None;
        }
        // Find the last section whose start is at or before `row`.
        let section = match self.starts.binary_search(&row) {
            Ok(exact) => exact,
            Err(insert) => insert - 1,
        };
        let offset = row - self.starts[section];
        Some(Location {
            section,
            // Offset 0 is the header; everything after is a line. Written as a
            // match rather than `then_some`, which would evaluate `offset - 1`
            // even when `offset` is zero and underflow.
            line: if offset == 0 { None } else { Some(offset - 1) },
        })
    }

    /// The number of sections indexed.
    pub fn section_count(&self) -> usize {
        self.starts.len()
    }

    /// Whether the list holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Every row in order. Intended for tests and for `copy all`, never for
    /// rendering — it defeats the point of the index.
    pub fn rows(&self) -> impl Iterator<Item = Location> + '_ {
        (0..self.count).filter_map(move |row| self.location(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never(_: usize) -> bool {
        false
    }

    fn collapse_all(collapsed: &[bool]) -> impl Fn(usize) -> bool + use<'_> {
        move |section| collapsed.get(section).copied().unwrap_or(false)
    }

    #[test]
    fn an_empty_list_has_no_rows() {
        let layout = DiffRowLayout::new(&[], &never);
        assert_eq!(layout.count(), 0);
        assert!(layout.is_empty());
        assert_eq!(layout.location(0), None);
    }

    #[test]
    fn an_empty_file_still_contributes_its_header() {
        let layout = DiffRowLayout::new(&[0, 0], &never);
        assert_eq!(layout.count(), 2, "two headers, no lines");
        assert_eq!(layout.header_row(0), Some(0));
        assert_eq!(layout.header_row(1), Some(1));
    }

    #[test]
    fn each_section_occupies_a_header_plus_its_lines() {
        let layout = DiffRowLayout::new(&[2, 3], &never);
        // file0: header + 2 lines = 3; file1: header + 3 lines = 4.
        assert_eq!(layout.count(), 7);
        assert_eq!(layout.header_row(0), Some(0));
        assert_eq!(layout.header_row(1), Some(3));
    }

    #[test]
    fn a_collapsed_section_contributes_only_its_header() {
        let layout = DiffRowLayout::new(&[2, 3, 1], &collapse_all(&[false, true, false]));
        // 3 + 1 + 2
        assert_eq!(layout.count(), 6);
        assert_eq!(layout.header_row(0), Some(0));
        assert_eq!(layout.header_row(1), Some(3), "file1 collapsed to one row");
        assert_eq!(layout.header_row(2), Some(4));
    }

    #[test]
    fn a_header_row_resolves_to_no_line() {
        let layout = DiffRowLayout::new(&[2], &never);
        assert_eq!(
            layout.location(0),
            Some(Location {
                section: 0,
                line: None
            })
        );
    }

    #[test]
    fn a_line_row_resolves_to_its_section_and_line() {
        let layout = DiffRowLayout::new(&[2, 3], &never);
        assert_eq!(
            layout.location(1),
            Some(Location {
                section: 0,
                line: Some(0)
            })
        );
        assert_eq!(
            layout.location(2),
            Some(Location {
                section: 0,
                line: Some(1)
            })
        );
        assert_eq!(
            layout.location(3),
            Some(Location {
                section: 1,
                line: None
            })
        );
        assert_eq!(
            layout.location(4),
            Some(Location {
                section: 1,
                line: Some(0)
            })
        );
        assert_eq!(
            layout.location(6),
            Some(Location {
                section: 1,
                line: Some(2)
            })
        );
    }

    #[test]
    fn rows_past_the_end_resolve_to_nothing() {
        let layout = DiffRowLayout::new(&[2], &never);
        assert_eq!(layout.location(3), None, "count is 3, so row 3 is outside");
    }

    #[test]
    fn an_unknown_section_has_no_header_row() {
        let layout = DiffRowLayout::new(&[1, 1], &never);
        assert_eq!(layout.header_row(2), None);
    }

    #[test]
    fn walking_every_row_round_trips_back_to_every_row() {
        let layout = DiffRowLayout::new(&[3, 0, 5, 1], &never);
        let walked: Vec<_> = layout.rows().collect();
        assert_eq!(walked.len(), layout.count());
        for (row, location) in walked.iter().enumerate() {
            assert_eq!(layout.location(row), Some(*location), "row {row}");
        }
    }

    #[test]
    fn headers_are_reachable_from_every_walked_position() {
        let layout = DiffRowLayout::new(&[2, 1, 4], &never);
        for section in 0..3 {
            let row = layout.header_row(section).unwrap();
            let location = layout.location(row).unwrap();
            assert_eq!(location.section, section);
            assert_eq!(location.line, None);
        }
    }

    #[test]
    fn a_hundred_thousand_rows_stay_addressable() {
        // The property the index exists for: the cost of locating a row must
        // not depend on how many rows there are.
        let counts: Vec<usize> = (0..5_000).map(|_| 20).collect();
        let layout = DiffRowLayout::new(&counts, &never);
        assert_eq!(layout.count(), 5_000 * 21);

        let last = layout.count() - 1;
        let location = layout.location(last).unwrap();
        assert_eq!(location.section, 4_999);
        assert_eq!(location.line, Some(19));
    }

    #[test]
    fn collapsing_everything_leaves_one_row_per_file() {
        let all_collapsed = collapse_all(&[true; 3]);
        let layout = DiffRowLayout::new(&[100, 200, 300], &all_collapsed);
        assert_eq!(layout.count(), 3);
        assert_eq!(layout.header_row(2), Some(2));
        assert_eq!(
            layout.location(2),
            Some(Location {
                section: 2,
                line: None
            })
        );
    }

    #[test]
    fn the_section_count_matches_the_input() {
        let layout = DiffRowLayout::new(&[1, 2, 3, 4, 5], &never);
        assert_eq!(layout.section_count(), 5);
    }
}
