use serde::{Deserialize, Serialize};

/// One path's status in the working tree.
///
/// `staged_status` and `unstaged_status` are the two porcelain status
/// characters for this path. A space means "no change at this level", so both
/// being `?` means untracked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: String,
    #[serde(rename = "stagedStatus")]
    pub staged_status: String,
    #[serde(rename = "unstagedStatus")]
    pub unstaged_status: String,
    #[serde(rename = "stagedAdditions", default)]
    pub staged_additions: usize,
    #[serde(rename = "stagedDeletions", default)]
    pub staged_deletions: usize,
    #[serde(rename = "unstagedAdditions", default)]
    pub unstaged_additions: usize,
    #[serde(rename = "unstagedDeletions", default)]
    pub unstaged_deletions: usize,
    /// Where a rename or copy came from. Committing only the destination would
    /// leave the source's deletion behind.
    #[serde(
        rename = "originalPath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub original_path: Option<String>,
}

impl GitFileStatus {
    pub fn new(
        path: impl Into<String>,
        staged_status: impl Into<String>,
        unstaged_status: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            staged_status: staged_status.into(),
            unstaged_status: unstaged_status.into(),
            staged_additions: 0,
            staged_deletions: 0,
            unstaged_additions: 0,
            unstaged_deletions: 0,
            original_path: None,
        }
    }

    /// The two status characters concatenated, or `?` when both are blank.
    pub fn status(&self) -> String {
        let combined = format!("{}{}", self.staged_status, self.unstaged_status);
        let trimmed = combined.trim();
        if trimmed.is_empty() {
            "?".to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn additions(&self) -> usize {
        self.staged_additions + self.unstaged_additions
    }

    pub fn deletions(&self) -> usize {
        self.staged_deletions + self.unstaged_deletions
    }

    pub fn has_staged_changes(&self) -> bool {
        self.staged_status != " " && self.staged_status != "?"
    }

    pub fn is_staged(&self) -> bool {
        self.has_staged_changes()
    }

    pub fn is_untracked(&self) -> bool {
        self.staged_status == "?" && self.unstaged_status == "?"
    }

    pub fn has_unstaged_changes(&self) -> bool {
        // An untracked file counts as unstaged work: it is not in the index and
        // not committed, so it is exactly what `git commit` would not pick up.
        self.is_untracked() || (self.unstaged_status != " " && self.unstaged_status != "?")
    }
}

/// A snapshot of a repository's state.
///
/// `revision` is the identity of the whole snapshot. Equal revisions mean equal
/// information, so an unchanged poll can skip a deep comparison of `files` —
/// which matters because the file list is the expensive part.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GitRepositoryInfo {
    #[serde(rename = "isGitRepo", default)]
    pub is_git_repo: bool,
    #[serde(default)]
    pub branch: String,
    #[serde(rename = "upstreamBranch", default)]
    pub upstream_branch: String,
    #[serde(rename = "hasRemote", default)]
    pub has_remote: bool,
    #[serde(rename = "hasCommits", default)]
    pub has_commits: bool,
    #[serde(rename = "aheadCount", default)]
    pub ahead_count: usize,
    #[serde(rename = "behindCount", default)]
    pub behind_count: usize,
    /// The branch tracks an upstream the remote no longer has, so status
    /// reports no ahead count even though every local commit is unpublished.
    #[serde(rename = "upstreamGone", default)]
    pub upstream_gone: bool,
    #[serde(default)]
    pub additions: usize,
    #[serde(default)]
    pub deletions: usize,
    #[serde(rename = "filesChanged", default)]
    pub files_changed: usize,
    #[serde(rename = "stagedFileCount", default)]
    pub staged_file_count: usize,
    #[serde(rename = "unstagedFileCount", default)]
    pub unstaged_file_count: usize,
    #[serde(rename = "statusFileCount", default)]
    pub status_file_count: usize,
    #[serde(rename = "contentRevision", default)]
    pub content_revision: u64,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub files: Vec<GitFileStatus>,
}

impl GitRepositoryInfo {
    /// The value used when a path is not a repository at all.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn has_changes(&self) -> bool {
        self.status_file_count > 0
    }

    /// Porcelain reports a detached HEAD as `HEAD (no branch)`.
    pub fn is_detached_head(&self) -> bool {
        self.branch.starts_with("HEAD (")
    }

    /// Whether a push would do anything useful.
    pub fn can_push(&self) -> bool {
        self.is_git_repo
            && self.has_remote
            && !self.is_detached_head()
            && (self.ahead_count > 0
                || (self.has_commits && (self.upstream_branch.is_empty() || self.upstream_gone)))
    }

    /// How many files have unstaged work.
    ///
    /// Convenience for the inspector header, which shows one number rather than
    /// a list.
    pub fn unstaged_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.has_unstaged_changes())
            .count()
    }

    /// Derive the counts from the file list.
    ///
    /// Kept separate from parsing so a snapshot assembled from several sources
    /// can be re-derived rather than trusted.
    pub fn recount(&mut self) {
        self.staged_file_count = self.files.iter().filter(|f| f.is_staged()).count();
        self.unstaged_file_count = self
            .files
            .iter()
            .filter(|f| f.has_unstaged_changes())
            .count();
        self.status_file_count = self.files.len();
        self.files_changed = self.files.len();
        self.additions = self.files.iter().map(GitFileStatus::additions).sum();
        self.deletions = self.files.iter().map(GitFileStatus::deletions).sum();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_mark_pair_reads_as_untracked() {
        // Porcelain spells an untracked path `??`.
        let f = GitFileStatus::new("a.txt", "?", "?");
        assert_eq!(f.status(), "??");
        assert!(f.is_untracked());
        assert!(f.has_unstaged_changes(), "untracked work is not staged");
        assert!(!f.has_staged_changes());
    }

    #[test]
    fn an_all_blank_pair_reads_as_unknown_rather_than_untracked() {
        // Two spaces means "tracked and unmodified", which porcelain never
        // actually emits — but if it did, it must not look like a new file.
        let f = GitFileStatus::new("a.txt", " ", " ");
        assert_eq!(f.status(), "?");
        assert!(!f.is_untracked());
        assert!(!f.has_unstaged_changes());
    }

    #[test]
    fn a_modified_staged_file_reads_as_both_staged_and_clean() {
        // `M ` means staged modification with nothing left unstaged.
        let f = GitFileStatus::new("a.txt", "M", " ");
        assert_eq!(f.status(), "M");
        assert!(f.is_staged());
        assert!(!f.has_unstaged_changes());
    }

    #[test]
    fn a_both_staged_and_modified_file_reports_both() {
        // `MM` means a staged change plus further unstaged edits.
        let f = GitFileStatus::new("a.txt", "M", "M");
        assert_eq!(f.status(), "MM");
        assert!(f.is_staged());
        assert!(f.has_unstaged_changes());
    }

    #[test]
    fn additions_and_deletions_sum_across_stages() {
        let mut f = GitFileStatus::new("a.txt", "M", "M");
        f.staged_additions = 3;
        f.staged_deletions = 1;
        f.unstaged_additions = 4;
        f.unstaged_deletions = 2;
        assert_eq!(f.additions(), 7);
        assert_eq!(f.deletions(), 3);
    }

    #[test]
    fn an_empty_snapshot_is_not_a_repository() {
        let info = GitRepositoryInfo::empty();
        assert!(!info.is_git_repo);
        assert!(!info.has_changes());
        assert!(!info.can_push());
        assert_eq!(info.revision, 0);
    }

    #[test]
    fn pushing_needs_a_remote_a_branch_and_something_to_send() {
        let mut info = GitRepositoryInfo {
            is_git_repo: true,
            has_remote: true,
            has_commits: true,
            ahead_count: 3,
            ..Default::default()
        };
        assert!(info.can_push(), "ahead of upstream");

        info.ahead_count = 0;
        info.upstream_branch = "origin/main".into();
        assert!(!info.can_push(), "level with upstream");

        info.upstream_branch = String::new();
        assert!(
            info.can_push(),
            "no upstream yet, so the first push sets it"
        );

        info.has_remote = false;
        assert!(!info.can_push(), "no remote");
    }

    #[test]
    fn a_gone_upstream_is_pushable_once_there_is_a_commit() {
        // `git clone` of an empty repository tracks `origin/main` before the
        // remote has it, and status reports `[gone]` with no ahead count.
        let mut info = GitRepositoryInfo {
            is_git_repo: true,
            has_remote: true,
            upstream_branch: "origin/main".into(),
            upstream_gone: true,
            ..Default::default()
        };
        assert!(!info.can_push(), "nothing to push before the first commit");
        info.has_commits = true;
        assert!(info.can_push());
    }

    #[test]
    fn a_detached_head_cannot_push() {
        let info = GitRepositoryInfo {
            is_git_repo: true,
            has_remote: true,
            has_commits: true,
            ahead_count: 5,
            branch: "HEAD (no branch)".into(),
            ..Default::default()
        };
        assert!(info.is_detached_head());
        assert!(!info.can_push());
    }

    #[test]
    fn recounting_derives_every_aggregate_from_the_files() {
        let mut info = GitRepositoryInfo {
            is_git_repo: true,
            files: vec![
                GitFileStatus::new("a.txt", "M", " "),
                GitFileStatus::new("b.txt", "?", "?"),
                GitFileStatus::new("c.txt", " ", "M"),
            ],
            ..Default::default()
        };
        info.files[0].staged_additions = 5;
        info.files[0].staged_deletions = 1;
        info.files[2].unstaged_additions = 2;
        info.files[2].unstaged_deletions = 3;
        info.recount();

        assert_eq!(info.status_file_count, 3);
        assert_eq!(info.staged_file_count, 1, "only a.txt is staged");
        assert_eq!(
            info.unstaged_file_count, 2,
            "b.txt is untracked and c.txt is modified"
        );
        assert_eq!(info.additions, 7);
        assert_eq!(info.deletions, 4);
    }

    #[test]
    fn a_snapshot_round_trips() {
        let mut info = GitRepositoryInfo {
            is_git_repo: true,
            branch: "main".into(),
            upstream_branch: "origin/main".into(),
            has_commits: true,
            has_remote: true,
            ahead_count: 2,
            files: vec![GitFileStatus::new("src/main.rs", "M", "M")],
            ..Default::default()
        };
        info.recount();
        info.revision = 0xDEAD_BEEF;

        let json = serde_json::to_string(&info).unwrap();
        let back: GitRepositoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }
}
