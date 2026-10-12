//! Parsers for the git plumbing FlowX shells out to.
//!
//! These are pure functions over command output. Keeping them free of process
//! handling means the awkward parts of git's output format — NUL separation,
//! rename records carrying a second path, binary files reporting `-` counts —
//! are testable without a repository.

use std::collections::HashMap;

use crate::models::{GitFileStatus, GitRepositoryInfo};

/// One `XY path` record from porcelain status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub path: String,
    pub staged_status: String,
    pub unstaged_status: String,
}

/// Everything one `git status --porcelain=v1 --branch -z` read tells us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub branch: String,
    pub upstream_branch: String,
    pub has_commits: bool,
    pub ahead_count: usize,
    pub behind_count: usize,
    pub files: Vec<StatusLine>,
    /// Some file has unstaged edits to a tracked path, so `git diff --numstat`
    /// has work to do.
    pub needs_unstaged_stats: bool,
    /// Some file has staged changes, so `git diff --cached --numstat` does.
    pub needs_staged_stats: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            branch: String::new(),
            upstream_branch: String::new(),
            // A successful read implies commits unless the header says otherwise.
            has_commits: true,
            ahead_count: 0,
            behind_count: 0,
            files: Vec::new(),
            needs_unstaged_stats: false,
            needs_staged_stats: false,
        }
    }
}

/// Line counts for one path from `--numstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineCounts {
    pub additions: usize,
    pub deletions: usize,
}

pub struct GitStatusParser;

impl GitStatusParser {
    /// Parse `git status --porcelain=v1 --branch -z`.
    pub fn parse_status(output: &str) -> Status {
        let mut snapshot = Status::default();
        let records: Vec<&str> = output.split('\0').filter(|r| !r.is_empty()).collect();
        let mut index = 0;

        while index < records.len() {
            let raw = records[index];

            if let Some(header) = raw.strip_prefix("## ") {
                Self::parse_branch_header(header, &mut snapshot);
                index += 1;
                continue;
            }

            // Every record is `XY path`; shorter than that is not a file.
            if raw.chars().count() < 3 {
                index += 1;
                continue;
            }

            let staged_status: String = raw.chars().next().into_iter().collect();
            let unstaged_status: String = raw.chars().nth(1).into_iter().collect();
            // The path starts after the two status characters and a space. Split
            // on bytes, not chars, so a multi-byte filename stays intact.
            let path = raw[3..].to_string();

            snapshot.needs_staged_stats |= staged_status != " " && staged_status != "?";
            snapshot.needs_unstaged_stats |= unstaged_status != " " && unstaged_status != "?";

            // A rename or copy record is followed by its source path as a
            // separate NUL-terminated record, which must be skipped.
            let is_rename_or_copy = ["R", "C"].contains(&staged_status.as_str())
                || ["R", "C"].contains(&unstaged_status.as_str());

            snapshot.files.push(StatusLine {
                path,
                staged_status,
                unstaged_status,
            });

            index += if is_rename_or_copy { 2 } else { 1 };
        }

        snapshot
    }

    /// Parse `git diff --numstat -z`.
    ///
    /// Each NUL-separated record is `adds<TAB>dels<TAB>path`. A binary file
    /// reports `-` for both counts but keeps its path in the same third field —
    /// verified against git rather than assumed — so a `-` simply parses as
    /// zero.
    pub fn parse_numstat(output: &str) -> HashMap<String, LineCounts> {
        output
            .split('\0')
            .filter(|record| !record.is_empty())
            .filter_map(|record| {
                let mut parts = record.splitn(3, '\t');
                let additions = parts.next()?.trim();
                let deletions = parts.next()?.trim();
                let path = parts.next()?;
                if path.is_empty() {
                    return None;
                }
                Some((
                    path.to_string(),
                    LineCounts {
                        additions: additions.parse().unwrap_or(0),
                        deletions: deletions.parse().unwrap_or(0),
                    },
                ))
            })
            .collect()
    }

    /// Assemble repository info from a successful status read plus its two
    /// optional numstat reads.
    pub fn make_info(
        status: &Status,
        unstaged_numstat: &HashMap<String, LineCounts>,
        staged_numstat: &HashMap<String, LineCounts>,
        has_remote: bool,
        content_revision: u64,
    ) -> GitRepositoryInfo {
        let files: Vec<GitFileStatus> = status
            .files
            .iter()
            .map(|line| {
                let unstaged = unstaged_numstat.get(&line.path);
                let staged = staged_numstat.get(&line.path);
                GitFileStatus {
                    path: line.path.clone(),
                    staged_status: line.staged_status.clone(),
                    unstaged_status: line.unstaged_status.clone(),
                    staged_additions: staged.map_or(0, |c| c.additions),
                    staged_deletions: staged.map_or(0, |c| c.deletions),
                    unstaged_additions: unstaged.map_or(0, |c| c.additions),
                    unstaged_deletions: unstaged.map_or(0, |c| c.deletions),
                }
            })
            .collect();

        let mut info = GitRepositoryInfo {
            is_git_repo: true,
            branch: status.branch.clone(),
            upstream_branch: status.upstream_branch.clone(),
            has_commits: status.has_commits,
            ahead_count: status.ahead_count,
            behind_count: status.behind_count,
            has_remote,
            content_revision,
            files,
            ..Default::default()
        };
        info.recount();
        info.revision = Self::revision(content_revision, has_remote);
        info
    }

    /// Identity of a snapshot.
    ///
    /// Every field derives from the hashed git output plus remote presence, so
    /// equal revisions mean equal information. Never zero, because zero is the
    /// revision of the empty non-repository value.
    pub fn revision(content_revision: u64, has_remote: bool) -> u64 {
        let mut hash = content_revision ^ 0x9E37_79B9_7F4A_7C15;
        hash = hash.wrapping_mul(if has_remote { 0xA5 } else { 0x5A });
        if hash == 0 { 1 } else { hash }
    }

    fn parse_branch_header(header: &str, snapshot: &mut Status) {
        let trimmed = header.trim();

        // An unborn branch has no commits yet, so there is no upstream and no
        // ahead/behind to report.
        for prefix in ["No commits yet on ", "Initial commit on "] {
            if let Some(branch) = trimmed.strip_prefix(prefix) {
                snapshot.branch = branch.to_string();
                snapshot.has_commits = false;
                return;
            }
        }

        snapshot.has_commits = true;

        // The header is `branch...upstream [ahead 1, behind 2]`.
        let mut components = trimmed.splitn(2, " [");
        let branch_component = components.next().unwrap_or_default();
        match branch_component.split_once("...") {
            Some((branch, upstream)) => {
                snapshot.branch = branch.to_string();
                snapshot.upstream_branch = upstream.to_string();
            }
            None => snapshot.branch = branch_component.to_string(),
        }

        let Some(summary) = components.next() else {
            return;
        };
        let summary = summary.trim_matches(|c| c == '[' || c == ']');
        for part in summary.split(", ") {
            if let Some(count) = part.strip_prefix("ahead ") {
                snapshot.ahead_count = count.trim().parse().unwrap_or(0);
            } else if let Some(count) = part.strip_prefix("behind ") {
                snapshot.behind_count = count.trim().parse().unwrap_or(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Join records the way git's NUL terminators do, with a trailing NUL.
    fn z(records: &[&str]) -> String {
        let mut s = records.join("\0");
        s.push('\0');
        s
    }

    #[test]
    fn a_clean_working_tree_reports_only_a_branch() {
        let out = z(&["## main...origin/main"]);
        let s = GitStatusParser::parse_status(&out);
        assert_eq!(s.branch, "main");
        assert_eq!(s.upstream_branch, "origin/main");
        assert!(s.has_commits);
        assert!(s.files.is_empty());
        assert!(!s.needs_staged_stats);
        assert!(!s.needs_unstaged_stats);
    }

    #[test]
    fn status_characters_split_into_staged_and_unstaged() {
        let out = z(&[
            "## HEAD",
            "MM src/lib.rs",
            " M src/main.rs",
            "A  new.rs",
            "?? notes.txt",
        ]);
        let s = GitStatusParser::parse_status(&out);

        let paths: Vec<_> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/lib.rs", "src/main.rs", "new.rs", "notes.txt"]);
        assert_eq!(s.files[0].staged_status, "M");
        assert_eq!(s.files[0].unstaged_status, "M");
        assert_eq!(s.files[1].staged_status, " ");
        assert_eq!(s.files[2].staged_status, "A");
        assert_eq!(s.files[3].staged_status, "?");
    }

    #[test]
    fn a_rename_record_consumes_its_source_path() {
        // `R  old -> new` is followed by the source path as its own record.
        let out = z(&["## HEAD", "R  new/name.rs", "old/name.rs", " M other.rs"]);
        let s = GitStatusParser::parse_status(&out);

        assert_eq!(
            s.files.len(),
            2,
            "the source path leaked into the file list"
        );
        assert_eq!(s.files[0].path, "new/name.rs");
        assert_eq!(s.files[1].path, "other.rs");
    }

    #[test]
    fn a_copy_record_also_consumes_its_source_path() {
        let out = z(&["## HEAD", "C  copy.rs", "source.rs"]);
        let s = GitStatusParser::parse_status(&out);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].path, "copy.rs");
    }

    #[test]
    fn stats_are_only_requested_when_something_would_report_them() {
        let staged = GitStatusParser::parse_status(&z(&["## HEAD", "M  a.rs"]));
        assert!(staged.needs_staged_stats);
        assert!(
            !staged.needs_unstaged_stats,
            "a staged-only change has no unstaged work to count"
        );

        let unstaged = GitStatusParser::parse_status(&z(&["## HEAD", " M a.rs"]));
        assert!(!unstaged.needs_staged_stats);
        assert!(unstaged.needs_unstaged_stats);

        let untracked = GitStatusParser::parse_status(&z(&["## HEAD", "?? a.rs"]));
        assert!(
            !untracked.needs_staged_stats && !untracked.needs_unstaged_stats,
            "an untracked file appears in no diff"
        );
    }

    #[test]
    fn an_unborn_branch_reports_no_commits() {
        for header in [
            "## No commits yet on main",
            "## Initial commit on feature/x",
        ] {
            let s = GitStatusParser::parse_status(&z(&[header]));
            assert!(!s.has_commits, "{header}");
            let expected = header.rsplit(" on ").next().unwrap().to_string();
            assert_eq!(s.branch, expected, "{header}");
        }
    }

    #[test]
    fn ahead_and_behind_are_read_from_the_summary() {
        let out = z(&["## main...origin/main [ahead 3, behind 7]"]);
        let s = GitStatusParser::parse_status(&out);
        assert_eq!(s.ahead_count, 3);
        assert_eq!(s.behind_count, 7);
    }

    #[test]
    fn only_ahead_or_only_behind_parses() {
        let ahead_only = GitStatusParser::parse_status(&z(&["## main...origin/main [ahead 2]"]));
        assert_eq!((ahead_only.ahead_count, ahead_only.behind_count), (2, 0));

        let behind_only = GitStatusParser::parse_status(&z(&["## main...origin/main [behind 4]"]));
        assert_eq!((behind_only.ahead_count, behind_only.behind_count), (0, 4));
    }

    #[test]
    fn a_branch_with_no_upstream_reports_no_upstream() {
        let s = GitStatusParser::parse_status(&z(&["## feature/new"]));
        assert_eq!(s.branch, "feature/new");
        assert_eq!(s.upstream_branch, "");
    }

    #[test]
    fn a_detached_head_keeps_its_porcelain_spelling() {
        let s = GitStatusParser::parse_status(&z(&["## HEAD (no branch)"]));
        assert_eq!(s.branch, "HEAD (no branch)");
        assert!(s.branch.starts_with("HEAD ("));
    }

    #[test]
    fn numstat_pairs_counts_with_paths() {
        let out = z(&["3\t1\tsrc/a.rs", "0\t7\tsrc/b.rs"]);
        let counts = GitStatusParser::parse_numstat(&out);
        assert_eq!(
            counts["src/a.rs"],
            LineCounts {
                additions: 3,
                deletions: 1
            }
        );
        assert_eq!(
            counts["src/b.rs"],
            LineCounts {
                additions: 0,
                deletions: 7
            }
        );
    }

    #[test]
    fn numstat_treats_a_binary_file_as_zero_counts() {
        // Verified against real git: a binary entry is `-\t-\t<path>` — the
        // path stays in the third field of the same record.
        let out = z(&["-\t-\tassets/logo.png", "2\t2\tsrc/a.rs"]);
        let counts = GitStatusParser::parse_numstat(&out);
        assert_eq!(
            counts["assets/logo.png"],
            LineCounts {
                additions: 0,
                deletions: 0
            }
        );
        assert_eq!(
            counts["src/a.rs"],
            LineCounts {
                additions: 2,
                deletions: 2
            }
        );
    }

    #[test]
    fn numstat_output_matches_what_git_actually_emits() {
        // Captured from `git diff --numstat -z` in a repository containing one
        // modified text file and one modified binary file.
        let captured = "-\t-\tblob.bin\x001\t0\ttext.txt\x00";
        let counts = GitStatusParser::parse_numstat(captured);
        assert_eq!(counts.len(), 2, "{counts:?}");
        assert_eq!(
            counts["blob.bin"],
            LineCounts {
                additions: 0,
                deletions: 0
            }
        );
        assert_eq!(
            counts["text.txt"],
            LineCounts {
                additions: 1,
                deletions: 0
            }
        );
    }

    #[test]
    fn numstat_survives_a_path_containing_a_tab() {
        // `splitn(3)` means only the first two tabs separate.
        let out = z(&["1\t2\todd\tname.rs"]);
        let counts = GitStatusParser::parse_numstat(&out);
        assert!(counts.contains_key("odd\tname.rs"), "{:?}", counts.keys());
    }

    #[test]
    fn numstat_ignores_malformed_records() {
        let out = z(&["garbage", "1\t2\tsrc/a.rs", "-\t-\t"]);
        let counts = GitStatusParser::parse_numstat(&out);
        assert_eq!(counts.len(), 1, "{counts:?}");
        assert!(counts.contains_key("src/a.rs"));
    }

    #[test]
    fn info_merges_counts_from_both_numstat_reads() {
        let status = GitStatusParser::parse_status(&z(&["## main", "MM src/lib.rs"]));
        let unstaged = HashMap::from([(
            "src/lib.rs".to_string(),
            LineCounts {
                additions: 4,
                deletions: 2,
            },
        )]);
        let staged = HashMap::from([(
            "src/lib.rs".to_string(),
            LineCounts {
                additions: 10,
                deletions: 1,
            },
        )]);

        let info = GitStatusParser::make_info(&status, &unstaged, &staged, true, 42);
        assert!(info.is_git_repo);
        assert_eq!(info.branch, "main");
        assert!(info.has_remote);

        let f = &info.files[0];
        assert_eq!(f.staged_additions, 10);
        assert_eq!(f.staged_deletions, 1);
        assert_eq!(f.unstaged_additions, 4);
        assert_eq!(f.unstaged_deletions, 2);
        assert_eq!(info.additions, 14);
        assert_eq!(info.deletions, 3);
    }

    #[test]
    fn info_falls_back_to_zero_counts_when_numstat_is_absent() {
        // An untracked path appears in no diff at all.
        let status = GitStatusParser::parse_status(&z(&["## main", "?? notes.txt"]));
        let info = GitStatusParser::make_info(&status, &HashMap::new(), &HashMap::new(), false, 1);
        let f = &info.files[0];
        assert_eq!((f.additions(), f.deletions()), (0, 0));
        assert_eq!(
            info.unstaged_file_count(),
            1,
            "an untracked file is unstaged work"
        );
    }

    #[test]
    fn info_counts_staged_and_unstaged_files_separately() {
        let status =
            GitStatusParser::parse_status(&z(&["## main", "M  a.rs", " M b.rs", "?? c.txt"]));
        let info = GitStatusParser::make_info(&status, &HashMap::new(), &HashMap::new(), false, 1);
        assert_eq!(info.status_file_count, 3);
        assert_eq!(info.staged_file_count, 1, "only a.rs is staged");
        assert_eq!(
            info.unstaged_file_count, 2,
            "b.rs modified and c.txt untracked"
        );
    }

    #[test]
    fn the_revision_is_never_zero() {
        assert_ne!(GitStatusParser::revision(0, false), 0);
        assert_ne!(GitStatusParser::revision(0, true), 0);
        assert_ne!(
            GitStatusParser::revision(u64::MAX, true),
            0,
            "a full-width input must not collide with the empty value"
        );
    }

    #[test]
    fn the_revision_depends_on_remote_presence() {
        assert_ne!(
            GitStatusParser::revision(1234, true),
            GitStatusParser::revision(1234, false)
        );
    }

    #[test]
    fn the_revision_is_stable_for_equal_input() {
        assert_eq!(
            GitStatusParser::revision(999, true),
            GitStatusParser::revision(999, true)
        );
    }

    #[test]
    fn status_ignores_records_shorter_than_a_file_entry() {
        let s = GitStatusParser::parse_status(&z(&["## main", "M", "x"]));
        assert!(s.files.is_empty(), "{:?}", s.files);
    }
}
