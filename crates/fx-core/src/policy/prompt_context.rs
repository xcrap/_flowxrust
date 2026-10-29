//! Explicit, editable references added to a prompt. The captured excerpt is
//! kept with its coordinates, so a later edit cannot silently change context.

use crate::git::{DiffLine, DiffSection};

/// A contiguous selection in the parsed diff, including its old/new coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSelection {
    pub path: String,
    pub anchor: usize,
    pub head: usize,
}

impl DiffSelection {
    pub fn contains(&self, path: &str, index: usize) -> bool {
        self.path == path
            && (self.anchor.min(self.head)..=self.anchor.max(self.head)).contains(&index)
    }

    pub fn excerpt(&self, section: &DiffSection, comparison: &str) -> Option<String> {
        if self.path != section.path {
            return None;
        }
        let lines = section
            .lines
            .get(self.anchor.min(self.head)..=self.anchor.max(self.head))?;
        if !lines
            .iter()
            .any(|line| line.old_line.is_some() || line.new_line.is_some())
        {
            return None;
        }
        let coordinates = |pick: fn(&DiffLine) -> Option<u32>| {
            let mut numbers = lines.iter().filter_map(pick);
            let first = numbers.next()?;
            let last = numbers.next_back().unwrap_or(first);
            Some(if first == last {
                first.to_string()
            } else {
                format!("{first}–{last}")
            })
        };
        let old = coordinates(|line| line.old_line).unwrap_or_else(|| "none".into());
        let new = coordinates(|line| line.new_line).unwrap_or_else(|| "none".into());
        let text = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "Referenced diff: {}\nComparison: {comparison}\nOld lines: {old}; new lines: {new}\n{}\n\nFeedback: ",
            quoted(&self.path),
            fenced(&text, "diff")
        ))
    }
}

pub fn file_reference(path: &str) -> String {
    format!("Referenced file: {}\n\n", quoted(path))
}

pub fn quoted_reply(text: &str) -> String {
    format!(
        "Quoted conversation text:\n{}\n\nFeedback: ",
        fenced(text, "text")
    )
}

fn quoted(text: &str) -> String {
    // JSON quoting keeps unusual file names on a single, unambiguous line.
    serde_json::to_string(text).expect("a string is valid JSON")
}

fn fenced(text: &str, language: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{language}\n{text}\n{fence}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::parse_section;

    #[test]
    fn reversed_selection_keeps_both_coordinate_systems_and_exact_text() {
        let section = parse_section(
            Some("a.rs".into()),
            ["@@ -10,2 +20,2 @@", "-old", "+new", " keep"],
        )
        .unwrap();
        let selection = DiffSelection {
            path: "a.rs".into(),
            anchor: 3,
            head: 1,
        };
        let excerpt = selection.excerpt(&section, "Unstaged").unwrap();
        assert!(excerpt.contains("Old lines: 10–11; new lines: 20–21"));
        assert!(excerpt.contains("-old\n+new\n keep"));
        assert!(!selection.contains("other.rs", 2));
    }

    #[test]
    fn deleted_lines_and_embedded_fences_are_unambiguous() {
        let section = parse_section(Some("notes.md".into()), ["@@ -7 +7,0 @@", "-```"]).unwrap();
        let selection = DiffSelection {
            path: "notes.md".into(),
            anchor: 1,
            head: 1,
        };
        let excerpt = selection.excerpt(&section, "Staged").unwrap();
        assert!(excerpt.contains("Old lines: 7; new lines: none"));
        assert!(excerpt.contains("````diff\n-```\n````"));
        assert!(file_reference("a\n\"b").contains("a\\n\\\"b"));
    }
}
