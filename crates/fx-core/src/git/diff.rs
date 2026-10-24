//! Unified-diff parsing for the diff viewer.
//!
//! A diff line cannot be classified by what it looks like. A removed SQL
//! comment `-- note` arrives as `--- note`, an added `++i` as `+++i`, and both
//! read exactly like file headers. What decides is *where* the line sits: inside
//! a hunk, the `@@` header says how many old and new lines follow, and every one
//! of them is content whose first character is only a marker. Outside a hunk,
//! everything is metadata. The parser therefore tracks hunk state rather than
//! matching prefixes, which is the port of the Swift `parseDiff` counters.
//!
//! Split rows are indices into a section's lines rather than copies of them, so
//! offering both layouts costs one small vector per file.

use std::borrow::Cow;

/// What one diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// File-level information outside a hunk: modes, renames, binary notices.
    Meta,
    /// An `@@ -a,b +c,d @@` header.
    Hunk,
    Context,
    Addition,
    Deletion,
}

/// One line of a parsed diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// The line as git printed it, marker included.
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

impl DiffLine {
    /// The source text without its one-character marker. Metadata and hunk
    /// headers have no marker and are returned whole.
    pub fn content(&self) -> &str {
        match self.kind {
            DiffLineKind::Context | DiffLineKind::Addition | DiffLineKind::Deletion => {
                // The marker is ASCII, so byte 1 is always a boundary.
                self.text.get(1..).unwrap_or_default()
            }
            DiffLineKind::Meta | DiffLineKind::Hunk => &self.text,
        }
    }
}

/// One side of a split row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSide {
    Empty,
    Context,
    Addition,
    Deletion,
}

/// One row of the side-by-side layout, as indices into [`DiffSection::lines`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRow {
    /// A metadata or hunk line spanning both columns.
    Annotation(usize),
    /// The old and new halves. A context line fills both with the same index;
    /// a deletion run is paired with the addition run that follows it.
    Pair {
        old: Option<usize>,
        new: Option<usize>,
    },
}

/// One file's part of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSection {
    /// Repository-relative path of the file after the change.
    pub path: String,
    pub lines: Vec<DiffLine>,
    pub split_rows: Vec<SplitRow>,
    pub additions: usize,
    pub deletions: usize,
    pub max_line_number: u32,
    /// The line with the most display columns. A virtualized list measures one
    /// row to size its content, and this is the row that must fit.
    pub widest_line: usize,
    /// That line's display columns, measured once here rather than per frame.
    pub widest_columns: usize,
}

impl DiffSection {
    /// Build a section from already classified lines.
    pub fn from_lines(path: impl Into<String>, lines: Vec<DiffLine>) -> Self {
        let mut additions = 0;
        let mut deletions = 0;
        let mut max_line_number = 0;
        let mut widest = (0, 0);
        for (index, line) in lines.iter().enumerate() {
            match line.kind {
                DiffLineKind::Addition => additions += 1,
                DiffLineKind::Deletion => deletions += 1,
                _ => {}
            }
            max_line_number = max_line_number
                .max(line.old_line.unwrap_or(0))
                .max(line.new_line.unwrap_or(0));
            let columns = display_columns(&line.text, MAXIMUM_MEASURED_COLUMNS);
            if columns > widest.1 {
                widest = (index, columns);
            }
        }
        let split_rows = split_rows(&lines);
        Self {
            path: path.into(),
            lines,
            split_rows,
            additions,
            deletions,
            max_line_number,
            widest_line: widest.0,
            widest_columns: widest.1,
        }
    }

    /// The side a split half belongs to.
    pub fn split_side(&self, index: Option<usize>) -> SplitSide {
        match index
            .and_then(|index| self.lines.get(index))
            .map(|line| line.kind)
        {
            Some(DiffLineKind::Addition) => SplitSide::Addition,
            Some(DiffLineKind::Deletion) => SplitSide::Deletion,
            Some(_) => SplitSide::Context,
            None => SplitSide::Empty,
        }
    }
}

/// Column ceiling when looking for the widest line. Beyond this a line is
/// truncated for display anyway, so measuring further buys nothing.
const MAXIMUM_MEASURED_COLUMNS: usize = 16_400;

/// Parse a multi-file `git diff`.
///
/// Text before the first `diff --git` header belongs to no file and is
/// dropped. A section that yields no lines at all is dropped too, so a header
/// with nothing under it never becomes an empty file.
pub fn parse_unified_diff(text: &str) -> Vec<DiffSection> {
    let mut sections = Vec::new();
    let mut current: Option<(Option<String>, Vec<&str>)> = None;
    for line in diff_lines(text) {
        if line.starts_with("diff --git ") {
            if let Some((path, lines)) = current.take() {
                sections.extend(parse_section(path, lines));
            }
            current = Some((anchor_path(line), Vec::new()));
            continue;
        }
        if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some((path, lines)) = current {
        sections.extend(parse_section(path, lines));
    }
    sections
}

/// Classify one file's lines by hunk state.
///
/// `fallback_path` comes from the `diff --git` header. `+++ b/…`, `rename to`
/// and `copy to` override it, because they name the destination without the
/// ambiguity of a header whose paths contain spaces.
pub fn parse_section<'a>(
    fallback_path: Option<String>,
    raw_lines: impl IntoIterator<Item = &'a str>,
) -> Option<DiffSection> {
    let mut lines = Vec::new();
    let mut path = fallback_path;
    let mut old_line: Option<u32> = None;
    let mut new_line: Option<u32> = None;
    // Lines still expected in the current hunk, from its `@@` counts.
    let mut remaining_old = 0usize;
    let mut remaining_new = 0usize;

    let mut push = |kind, text: &str, old, new| {
        lines.push(DiffLine {
            kind,
            text: text.to_string(),
            old_line: old,
            new_line: new,
        })
    };

    for raw in raw_lines {
        if raw.starts_with("@@") {
            let hunk = hunk_header(raw);
            old_line = hunk.map(|h| h.old_start);
            new_line = hunk.map(|h| h.new_start);
            // An unparseable header still classifies what follows by marker
            // until the next header.
            remaining_old = hunk.map_or(usize::MAX, |h| h.old_count as usize);
            remaining_new = hunk.map_or(usize::MAX, |h| h.new_count as usize);
            push(DiffLineKind::Hunk, raw, None, None);
            continue;
        }

        if remaining_old > 0 || remaining_new > 0 {
            match raw.as_bytes().first() {
                Some(b'+') => {
                    push(DiffLineKind::Addition, raw, None, new_line);
                    new_line = new_line.map(|n| n + 1);
                    remaining_new = remaining_new.saturating_sub(1);
                    continue;
                }
                Some(b'-') => {
                    push(DiffLineKind::Deletion, raw, old_line, None);
                    old_line = old_line.map(|n| n + 1);
                    remaining_old = remaining_old.saturating_sub(1);
                    continue;
                }
                // `diff.suppressBlankEmpty` prints an empty context line with
                // no marker at all.
                Some(b' ') | None => {
                    push(DiffLineKind::Context, raw, old_line, new_line);
                    old_line = old_line.map(|n| n + 1);
                    new_line = new_line.map(|n| n + 1);
                    remaining_old = remaining_old.saturating_sub(1);
                    remaining_new = remaining_new.saturating_sub(1);
                    continue;
                }
                // `\ No newline at end of file` belongs to the line before it
                // and must not break a deletion/addition pair in split view.
                Some(b'\\') => continue,
                _ => {
                    remaining_old = 0;
                    remaining_new = 0;
                }
            }
        }

        // Outside a hunk everything is file metadata.
        if let Some(destination) = raw.strip_prefix("+++ ") {
            if let Some(destination) = header_path(destination) {
                path = Some(destination);
            }
            continue;
        }
        if let Some(destination) = raw
            .strip_prefix("rename to ")
            .or_else(|| raw.strip_prefix("copy to "))
        {
            path = Some(unquote(destination).into_owned());
        }
        if raw.is_empty()
            || raw.starts_with("index ")
            || raw.starts_with("--- ")
            || raw.starts_with("\\ No newline")
        {
            continue;
        }
        push(DiffLineKind::Meta, raw, None, None);
    }

    let path = path.filter(|path| !path.is_empty())?;
    (!lines.is_empty()).then(|| DiffSection::from_lines(path, lines))
}

/// Split on LF and drop a trailing CR, so a CRLF file does not gain a phantom
/// character per row, and a Unicode line separator inside content does not
/// split a diff line.
pub fn diff_lines(text: &str) -> impl Iterator<Item = &str> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(move |_| !text.is_empty())
}

/// The counts in an `@@ -a,b +c,d @@` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkHeader {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

/// Parse a hunk header. A missing count means one line.
pub fn hunk_header(line: &str) -> Option<HunkHeader> {
    let mut parts = line.split(' ');
    if parts.next() != Some("@@") {
        return None;
    }
    let range = |part: Option<&str>, marker: char| -> Option<(u32, u32)> {
        let part = part?.strip_prefix(marker)?;
        let mut numbers = part.splitn(2, ',');
        let start = numbers.next()?.parse().ok()?;
        let count = numbers.next().map_or(Some(1), |count| count.parse().ok())?;
        Some((start, count))
    };
    let (old_start, old_count) = range(parts.next(), '-')?;
    let (new_start, new_count) = range(parts.next(), '+')?;
    Some(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// Pair deletion runs with the addition runs that follow them.
pub fn split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    let mut rows = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        match lines[index].kind {
            DiffLineKind::Meta | DiffLineKind::Hunk => {
                rows.push(SplitRow::Annotation(index));
                index += 1;
            }
            DiffLineKind::Context => {
                rows.push(SplitRow::Pair {
                    old: Some(index),
                    new: Some(index),
                });
                index += 1;
            }
            DiffLineKind::Addition => {
                rows.push(SplitRow::Pair {
                    old: None,
                    new: Some(index),
                });
                index += 1;
            }
            DiffLineKind::Deletion => {
                let deletions = run(lines, index, DiffLineKind::Deletion);
                let additions = run(lines, deletions.end, DiffLineKind::Addition);
                index = additions.end;
                for offset in 0..deletions.len().max(additions.len()) {
                    rows.push(SplitRow::Pair {
                        old: (offset < deletions.len()).then(|| deletions.start + offset),
                        new: (offset < additions.len()).then(|| additions.start + offset),
                    });
                }
            }
        }
    }
    rows
}

fn run(lines: &[DiffLine], start: usize, kind: DiffLineKind) -> std::ops::Range<usize> {
    let length = lines[start..]
        .iter()
        .take_while(|line| line.kind == kind)
        .count();
    start..start + length
}

/// Monospace cells a line occupies: tabs to the next stop of four, and two
/// cells for anything outside ASCII so fallback glyphs never overlap.
pub fn display_columns(text: &str, maximum: usize) -> usize {
    let mut columns = 0;
    for character in text.chars() {
        if character == '\t' {
            columns += 4 - (columns % 4);
        } else if character.is_ascii() {
            columns += 1;
        } else {
            columns += 2;
        }
        if columns >= maximum {
            return maximum;
        }
    }
    columns
}

/// The path a `diff --git` header names.
///
/// Git leaves a path containing spaces unquoted (`a/My Notes.md b/My
/// Notes.md`), so splitting on whitespace would yield `Notes.md`. When both
/// sides name the same path the header is exactly `a/P b/P`, which can be split
/// by length; otherwise the two tokens are read, quoted or not.
pub fn anchor_path(line: &str) -> Option<String> {
    let header = line.strip_prefix("diff --git ")?;
    if header.starts_with("a/") && header.len() > 5 && (header.len() - 5) % 2 == 0 {
        let length = (header.len() - 5) / 2;
        if let (Some(old), Some(rest)) = (header.get(2..2 + length), header.get(2 + length..))
            && rest.strip_prefix(" b/") == Some(old)
        {
            return Some(old.to_string());
        }
    }
    let mut cursor = header;
    let old = path_token(&mut cursor)?;
    let new = path_token(&mut cursor)?;
    // A deletion names /dev/null on the new side; otherwise the b/ path is
    // authoritative, including rename destinations.
    let chosen = if new == "/dev/null" { old } else { new };
    let path = chosen
        .strip_prefix("a/")
        .or_else(|| chosen.strip_prefix("b/"))
        .unwrap_or(&chosen);
    (!path.is_empty()).then(|| path.to_string())
}

/// The path in a `+++ b/…` line, or `None` for `/dev/null`.
fn header_path(value: &str) -> Option<String> {
    // Git terminates a name containing a space with a tab, for patch(1).
    let value = value.strip_suffix('\t').unwrap_or(value);
    let value = unquote(value);
    if value == "/dev/null" {
        return None;
    }
    let path = value.strip_prefix("b/").unwrap_or(&value);
    (!path.is_empty()).then(|| path.to_string())
}

/// Read one possibly quoted path token and advance past it.
fn path_token(cursor: &mut &str) -> Option<String> {
    let text = cursor.trim_start();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('"') {
        let (value, consumed) = unquote_prefix(text)?;
        *cursor = &text[consumed..];
        return Some(value);
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    *cursor = &text[end..];
    Some(text[..end].to_string())
}

/// Undo git's C-style quoting when the value is quoted.
pub fn unquote(value: &str) -> Cow<'_, str> {
    if value.starts_with('"')
        && let Some((unquoted, consumed)) = unquote_prefix(value)
        && consumed == value.len()
    {
        return Cow::Owned(unquoted);
    }
    Cow::Borrowed(value)
}

/// Decode a quoted string at the start of `text`, returning it and the number
/// of bytes consumed. Octal escapes are raw bytes, so a UTF-8 name arrives as
/// several of them and is decoded once at the end.
fn unquote_prefix(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut decoded = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        match byte {
            b'"' => return Some((String::from_utf8_lossy(&decoded).into_owned(), index)),
            b'\\' => {
                let escaped = *bytes.get(index)?;
                index += 1;
                if (b'0'..=b'7').contains(&escaped) {
                    let mut value = u32::from(escaped - b'0');
                    for _ in 0..2 {
                        match bytes.get(index) {
                            Some(digit @ b'0'..=b'7') => {
                                value = value * 8 + u32::from(digit - b'0');
                                index += 1;
                            }
                            _ => break,
                        }
                    }
                    decoded.push(value as u8);
                    continue;
                }
                decoded.push(match escaped {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => 9,
                    b'n' => 10,
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => 13,
                    other => other,
                });
            }
            other => decoded.push(other),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(section: &DiffSection) -> Vec<(DiffLineKind, &str)> {
        section
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }

    #[test]
    fn hunk_content_that_looks_like_a_header_is_still_content() {
        // A removed SQL comment, an added pre-increment, and a context line
        // ending in " changes" — every one used to be drawn as metadata.
        let diff = "\
diff --git a/query.sql b/query.sql
index 1111111..2222222 100644
--- a/query.sql
+++ b/query.sql
@@ -1,3 +1,3 @@
--- comment
+++i;
 3 changes
-@@ not a header
+diff --git inside content
";
        let sections = parse_unified_diff(diff);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].path, "query.sql");
        assert_eq!(
            kinds(&sections[0]),
            [
                (DiffLineKind::Hunk, "@@ -1,3 +1,3 @@"),
                (DiffLineKind::Deletion, "--- comment"),
                (DiffLineKind::Addition, "+++i;"),
                (DiffLineKind::Context, " 3 changes"),
                (DiffLineKind::Deletion, "-@@ not a header"),
                (DiffLineKind::Addition, "+diff --git inside content"),
            ]
        );
        assert_eq!((sections[0].additions, sections[0].deletions), (2, 2));
    }

    #[test]
    fn a_line_inside_content_that_starts_a_new_file_header_is_not_a_split() {
        // `diff --git` at column zero can only be a header, because content
        // always carries a marker.
        let diff = "\
diff --git a/one.txt b/one.txt
@@ -1 +1 @@
-a
+b
diff --git a/two.txt b/two.txt
@@ -0,0 +1 @@
+c
";
        let paths: Vec<_> = parse_unified_diff(diff)
            .into_iter()
            .map(|section| section.path)
            .collect();
        assert_eq!(paths, ["one.txt", "two.txt"]);
    }

    #[test]
    fn line_numbers_follow_the_hunk_header() {
        let diff = "\
diff --git a/a.rs b/a.rs
@@ -10,3 +10,4 @@ fn main() {
 keep
-old
+new
+extra
 tail
";
        let section = &parse_unified_diff(diff)[0];
        let numbers: Vec<_> = section
            .lines
            .iter()
            .map(|line| (line.old_line, line.new_line))
            .collect();
        assert_eq!(
            numbers,
            [
                (None, None),
                (Some(10), Some(10)),
                (Some(11), None),
                (None, Some(11)),
                (None, Some(12)),
                (Some(12), Some(13)),
            ]
        );
        assert_eq!(section.max_line_number, 13);
    }

    #[test]
    fn file_metadata_is_kept_and_headers_are_dropped() {
        let diff = "\
diff --git a/old name.txt b/new name.txt
similarity index 90%
rename from old name.txt
rename to new name.txt
index 1111111..2222222 100644
--- a/old name.txt
+++ b/new name.txt
@@ -1 +1 @@
-a
+b
diff --git a/logo.png b/logo.png
new file mode 100644
index 0000000..3333333
Binary files /dev/null and b/logo.png differ
";
        let sections = parse_unified_diff(diff);
        assert_eq!(sections[0].path, "new name.txt");
        assert_eq!(
            kinds(&sections[0])[..3],
            [
                (DiffLineKind::Meta, "similarity index 90%"),
                (DiffLineKind::Meta, "rename from old name.txt"),
                (DiffLineKind::Meta, "rename to new name.txt"),
            ]
        );
        assert_eq!(sections[1].path, "logo.png");
        assert_eq!(
            kinds(&sections[1]),
            [
                (DiffLineKind::Meta, "new file mode 100644"),
                (
                    DiffLineKind::Meta,
                    "Binary files /dev/null and b/logo.png differ"
                ),
            ]
        );
    }

    #[test]
    fn a_deleted_file_keeps_its_old_path() {
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        let section = &parse_unified_diff(diff)[0];
        assert_eq!(section.path, "gone.txt");
        assert_eq!(section.deletions, 1);
    }

    #[test]
    fn header_paths_with_spaces_quotes_and_escapes_resolve() {
        assert_eq!(
            anchor_path("diff --git a/My Notes.md b/My Notes.md").as_deref(),
            Some("My Notes.md")
        );
        assert_eq!(
            anchor_path(r#"diff --git "a/tab\there.txt" "b/tab\there.txt""#).as_deref(),
            Some("tab\there.txt")
        );
        assert_eq!(
            anchor_path(r#"diff --git "a/caf\303\251.txt" "b/caf\303\251.txt""#).as_deref(),
            Some("café.txt")
        );
        assert_eq!(
            anchor_path("diff --git a/old.rs b/new.rs").as_deref(),
            Some("new.rs")
        );
        // `+++` wins over an ambiguous header.
        let diff =
            "diff --git a/a b c b/a b c\n--- a/a b c\t\n+++ b/a b c\t\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(parse_unified_diff(diff)[0].path, "a b c");
    }

    #[test]
    fn crlf_content_and_missing_newline_markers_do_not_add_rows() {
        let diff = "diff --git a/w.txt b/w.txt\r\n@@ -1 +1 @@\r\n-old\r\n\\ No newline at end of file\r\n+new\r\n\\ No newline at end of file\r\n";
        let section = &parse_unified_diff(diff)[0];
        assert_eq!(
            kinds(section),
            [
                (DiffLineKind::Hunk, "@@ -1 +1 @@"),
                (DiffLineKind::Deletion, "-old"),
                (DiffLineKind::Addition, "+new"),
            ]
        );
        assert_eq!(
            section.split_rows,
            [
                SplitRow::Annotation(0),
                SplitRow::Pair {
                    old: Some(1),
                    new: Some(2)
                }
            ],
            "the marker between the pair must not break it"
        );
    }

    #[test]
    fn an_unparseable_hunk_header_still_classifies_by_marker() {
        let section = parse_section(
            Some("x".into()),
            [
                "@@ garbage @@",
                "+added",
                "-removed",
                " same",
                "trailing note",
            ],
        )
        .unwrap();
        assert_eq!(
            kinds(&section),
            [
                (DiffLineKind::Hunk, "@@ garbage @@"),
                (DiffLineKind::Addition, "+added"),
                (DiffLineKind::Deletion, "-removed"),
                (DiffLineKind::Context, " same"),
                (DiffLineKind::Meta, "trailing note"),
            ]
        );
        assert_eq!(
            section.lines[1].new_line, None,
            "no numbers without a header"
        );
    }

    #[test]
    fn deletion_runs_pair_with_the_additions_that_follow() {
        let section = parse_section(
            Some("x".into()),
            ["@@ -1,5 +1,3 @@", " a", "-b", "-c", "+B", " d", "-e"],
        )
        .unwrap();
        assert_eq!(
            section.split_rows,
            [
                SplitRow::Annotation(0),
                SplitRow::Pair {
                    old: Some(1),
                    new: Some(1)
                },
                SplitRow::Pair {
                    old: Some(2),
                    new: Some(4)
                },
                SplitRow::Pair {
                    old: Some(3),
                    new: None
                },
                SplitRow::Pair {
                    old: Some(5),
                    new: Some(5)
                },
                SplitRow::Pair {
                    old: Some(6),
                    new: None
                },
            ]
        );
        assert_eq!(section.split_side(Some(4)), SplitSide::Addition);
        assert_eq!(section.split_side(None), SplitSide::Empty);
        assert_eq!(section.lines[2].content(), "b");
        assert_eq!(section.lines[0].content(), "@@ -1,5 +1,3 @@");
    }

    #[test]
    fn the_widest_line_is_measured_in_display_columns() {
        let section = parse_section(
            Some("x".into()),
            ["@@ -0,0 +1,3 @@", "+short", "+\t\t\t\twide", "+ééé"],
        )
        .unwrap();
        assert_eq!(display_columns("+\t\t\t\twide", 100), 20);
        assert_eq!(display_columns("+ééé", 100), 7);
        assert_eq!(section.widest_line, 2);
        assert_eq!(section.widest_columns, 20);
        assert_eq!(display_columns(&"x".repeat(50), 10), 10);
    }

    #[test]
    fn a_header_with_nothing_under_it_is_not_a_file() {
        assert!(parse_unified_diff("diff --git a/x b/x\nindex 1..2\n").is_empty());
        assert!(parse_unified_diff("").is_empty());
        assert!(parse_unified_diff("stray text\n").is_empty());
    }
}
