//! The identity of one repository read.
//!
//! A poller that cannot tell whether anything changed has to repaint, and
//! reload every dependent diff, on every tick. The revision is what lets it
//! skip both: equal revisions mean the status, the line totals, and every
//! changed file on disk are as they were.

use std::path::Path;

use super::command::ContentRevisionBuilder;

/// The git output a snapshot was built from.
#[derive(Debug, Clone, Copy, Default)]
pub struct RevisionInputs<'a> {
    pub status: &'a str,
    pub unstaged_numstat: &'a str,
    pub staged_numstat: &'a str,
}

/// Hash a snapshot's inputs.
///
/// Status and numstat text alone cannot see a same-size rewrite of a changed
/// file, nor an untracked file whose lines changed, so each changed path's
/// metadata goes in as well. The index goes in because `git add` changes what
/// is staged without changing a byte in the working tree.
pub fn content_revision<'a>(
    root: &Path,
    git_dir: Option<&Path>,
    inputs: RevisionInputs<'_>,
    paths: impl IntoIterator<Item = &'a str>,
) -> u64 {
    let mut builder = ContentRevisionBuilder::new();
    builder
        .field("status", inputs.status)
        .field("unstaged", inputs.unstaged_numstat)
        .field("staged", inputs.staged_numstat);
    let mut paths: Vec<&str> = paths.into_iter().collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        let path = Path::new(path);
        // Never follow a link: its own metadata is what git reports on.
        let metadata = std::fs::symlink_metadata(root.join(path)).ok();
        builder.metadata(path, metadata.as_ref());
    }
    if let Some(git_dir) = git_dir {
        let index = git_dir.join("index");
        let metadata = std::fs::metadata(&index).ok();
        builder.metadata(&index, metadata.as_ref());
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revision(root: &Path, status: &str) -> u64 {
        content_revision(
            root,
            Some(&root.join(".git")),
            RevisionInputs {
                status,
                ..Default::default()
            },
            ["a.txt"],
        )
    }

    #[test]
    fn an_unchanged_tree_keeps_its_revision() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join("a.txt"), "one\n").unwrap();
        assert_eq!(
            revision(temp.path(), " M a.txt"),
            revision(temp.path(), " M a.txt")
        );
    }

    #[test]
    fn a_same_size_rewrite_changes_the_revision() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        let file = temp.path().join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let before = revision(temp.path(), " M a.txt");
        // Identical status text and size: only the file's metadata can tell.
        let replacement = temp.path().join("a.tmp");
        std::fs::write(&replacement, "two\n").unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        assert_ne!(before, revision(temp.path(), " M a.txt"));
    }

    #[test]
    fn staging_alone_changes_the_revision() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::write(temp.path().join("a.txt"), "one\n").unwrap();
        let before = revision(temp.path(), " M a.txt");
        std::fs::write(temp.path().join(".git/index"), "staged").unwrap();
        assert_ne!(before, revision(temp.path(), " M a.txt"));
    }

    #[test]
    fn a_vanished_file_differs_from_a_present_one() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "").unwrap();
        let present = revision(temp.path(), "");
        std::fs::remove_file(temp.path().join("a.txt")).unwrap();
        assert_ne!(present, revision(temp.path(), ""));
    }
}
