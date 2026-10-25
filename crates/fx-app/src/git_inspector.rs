//! Bounded Git reads for the inspector, and the worker every Git job runs on.
//!
//! Reads never refresh or lock the index (`GIT_OPTIONAL_LOCKS=0` in the
//! hardened environment), run without a pager or a filesystem monitor, and
//! read pathspecs literally. Diffs disable external diff and textconv
//! programs, so a repository's configuration cannot make a poll run a program.

use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use fx_core::git::{
    self, CommandOutput, CommandSpec, DiffSection, GitStatusParser, LineCounts, RevisionInputs,
};
use fx_core::{GitFileStatus, GitRepositoryInfo};

pub const MAX_FILES: usize = 2_000;
/// Ceiling on one new file, both for counting its lines and for showing it.
pub const MAX_NEW_FILE_BYTES: usize = 128 * 1024;
/// Ceiling on everything one diff shows.
pub const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;
/// New files beyond this many are listed but not read for the diff.
pub const MAX_NEW_FILES_IN_DIFF: usize = 200;
const MAX_LIST_BYTES: usize = 1024 * 1024;
const MAX_UNTRACKED_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// The one runtime every Git job runs on.
///
/// macOS dispatch workers have small stacks, and tokio's nested Git futures can
/// exceed them in debug builds, so a job is *constructed and polled* on this
/// runtime's workers, whose stack size is known, and GPUI only ever holds the
/// receiver. Sharing one runtime replaces an OS thread and a runtime per read,
/// which at a poll every five seconds was most of what polling cost.
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("flowx-git")
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .map_err(|error| format!("Cannot start the Git worker: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// A Git job in flight.
pub type Job<T> = Result<tokio::sync::oneshot::Receiver<Result<T, String>>, String>;

/// Run a Git job on the worker. The closure builds the future there, not on
/// the caller's stack.
pub fn spawn_git<T, F, Fut>(job: F) -> Job<T>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, String>> + Send + 'static,
{
    let runtime = runtime()?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        let _ = sender.send(job().await);
    });
    Ok(receiver)
}

pub async fn finish_git<T>(job: Job<T>) -> Result<T, String> {
    job?.await
        .map_err(|_| "The Git worker stopped unexpectedly. Use Refresh to retry.".to_string())?
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub root: PathBuf,
    pub repository: Option<GitRepositoryInfo>,
    pub notice: Option<String>,
}

impl Snapshot {
    /// Whether two reads would draw the same thing.
    pub fn same_as(&self, other: &Snapshot) -> bool {
        self.root == other.root
            && self.notice == other.notice
            && self.repository.as_ref().map(|repo| repo.revision)
                == other.repository.as_ref().map(|repo| repo.revision)
    }
}

/// Which two trees a diff compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ComparisonMode {
    /// The working tree against the index, plus new files.
    #[default]
    Unstaged,
    /// The index against `HEAD`.
    Staged,
    /// The working tree against `HEAD`, plus new files.
    Base,
}

impl ComparisonMode {
    pub const ALL: [Self; 3] = [Self::Unstaged, Self::Staged, Self::Base];

    pub fn title(self) -> &'static str {
        match self {
            Self::Unstaged => "Unstaged",
            Self::Staged => "Staged",
            Self::Base => "Base",
        }
    }

    pub fn includes(self, file: &GitFileStatus) -> bool {
        match self {
            Self::Unstaged => file.has_unstaged_changes(),
            Self::Staged => file.has_staged_changes(),
            Self::Base => true,
        }
    }

    /// The status letters that describe a file in this comparison.
    pub fn status(self, file: &GitFileStatus) -> String {
        match self {
            Self::Unstaged if file.is_untracked() => "?".into(),
            Self::Unstaged => file.unstaged_status.trim().to_string(),
            Self::Staged => file.staged_status.trim().to_string(),
            Self::Base => file.status(),
        }
    }

    pub fn empty_title(self) -> &'static str {
        match self {
            Self::Unstaged => "No unstaged changes",
            Self::Staged => "Nothing staged",
            Self::Base => "Working tree clean",
        }
    }

    pub fn empty_hint(self) -> &'static str {
        match self {
            Self::Unstaged => "Edit tracked files or add new files to see the local diff here.",
            Self::Staged => "Stage changes first to inspect the staged diff.",
            Self::Base => "File changes will appear here as you work.",
        }
    }
}

/// A parsed diff, ready to draw.
#[derive(Clone, Debug, Default)]
pub struct DiffDocument {
    pub sections: Vec<DiffSection>,
    /// Status letters for each section, in the same order.
    pub statuses: Vec<String>,
    pub notice: Option<String>,
}

async fn command(root: &Path, args: &[&str], cap: usize) -> Result<CommandOutput, String> {
    // Literal pathspecs prevent a filename such as :(glob)* from widening a
    // selected-file diff. External diff/textconv programs are disabled below.
    let args: Vec<String> = [
        "--no-pager",
        "--literal-pathspecs",
        "-c",
        "core.fsmonitor=false",
    ]
    .into_iter()
    .chain(args.iter().copied())
    .map(String::from)
    .collect();
    let mut spec = CommandSpec::new(&args)
        .in_directory(Some(root))
        .with_timeout(Duration::from_secs(10))
        .with_max_output_bytes(cap);
    spec.append_truncation_notice = false;
    git::run_lenient(spec)
        .await
        .map_err(|error| error.to_string())
}

fn require_success(output: CommandOutput) -> Result<CommandOutput, String> {
    if output.success() {
        Ok(output)
    } else {
        let detail = output
            .stderr
            .lines()
            .next()
            .unwrap_or("Git could not read this folder.");
        Err(format!(
            "{detail} Refresh after checking the folder and Git permissions."
        ))
    }
}

/// Read only the working-tree changes. Ordinary folders report no repository;
/// they are never recursively enumerated as a general-purpose file browser.
pub async fn changes_snapshot(root: &Path) -> Result<Snapshot, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("Cannot open this project: {error}"))?;
    if !root.is_dir() {
        return Err(
            "This project is no longer a folder. Choose an existing project folder.".into(),
        );
    }
    let probe = command(
        &root,
        &["rev-parse", "--show-toplevel", "--absolute-git-dir"],
        MAX_LIST_BYTES,
    )
    .await?;
    if !probe.success() {
        if !probe.stderr.contains("not a git repository") {
            return require_success(probe).map(|_| unreachable!());
        }
        return Ok(Snapshot {
            root,
            repository: None,
            notice: None,
        });
    }
    // Two lines: the top level, then the git directory. Split from the end so
    // a top level containing a newline still reads correctly.
    let probe = probe.stdout.trim_end_matches('\n');
    let (top_level, git_dir) = probe.rsplit_once('\n').unwrap_or((probe, ""));
    let root = PathBuf::from(top_level);
    let git_dir = (!git_dir.is_empty()).then(|| PathBuf::from(git_dir));
    let status_output = require_success(
        command(
            &root,
            &[
                "status",
                "--porcelain=v1",
                "--branch",
                "-z",
                "--untracked-files=all",
            ],
            MAX_LIST_BYTES,
        )
        .await?,
    )?;
    let status_text = complete_records(&status_output.stdout);
    let mut status = GitStatusParser::parse_status(status_text);
    let truncated = status_output.truncated || status.files.len() > MAX_FILES;
    status.files.truncate(MAX_FILES);
    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Changed-file lists and line totals are partial: limited to {MAX_FILES} entries and 1 MiB."
        ));
    }
    let (unstaged_read, staged_read, remotes) = tokio::join!(
        numstat(&root, false, status.needs_unstaged_stats),
        numstat(&root, true, status.needs_staged_stats),
        command(&root, &["remote"], MAX_LIST_BYTES),
    );
    let (mut unstaged, unstaged_text, unstaged_notice) = unstaged_read;
    let (staged, staged_text, staged_notice) = staged_read;
    notices.extend(unstaged_notice);
    notices.extend(staged_notice);
    let has_remote = !status.upstream_branch.is_empty()
        || remotes.is_ok_and(|output| output.success() && !output.stdout.trim().is_empty());
    let mut remaining = MAX_UNTRACKED_TOTAL_BYTES;
    let mut uncounted = 0;
    for file in &status.files {
        if file.staged_status != "?" || file.unstaged_status != "?" {
            continue;
        }
        // A nested repository is reported as `?? vendor/lib/`. It has no
        // lines of its own to count, so it must not mark the totals partial.
        if is_directory_entry(&root, &file.path) {
            continue;
        }
        match untracked_counts(&root, &file.path, &mut remaining) {
            Ok(counts) => {
                unstaged.insert(file.path.clone(), counts);
            }
            Err(_) => uncounted += 1,
        }
    }
    if uncounted > 0 {
        notices.push(format!(
            "Line totals are partial: {uncounted} new file(s) could not be counted within the read limits."
        ));
    }
    let revision = git::content_revision(
        &root,
        git_dir.as_deref(),
        RevisionInputs {
            status: status_text,
            unstaged_numstat: &unstaged_text,
            staged_numstat: &staged_text,
        },
        status.files.iter().map(|file| file.path.as_str()),
    );
    let info = GitStatusParser::make_info(&status, &unstaged, &staged, has_remote, revision);
    Ok(Snapshot {
        root,
        repository: Some(info),
        notice: (!notices.is_empty()).then(|| notices.join(" ")),
    })
}

/// Whether an untracked entry is a directory rather than a file: a trailing
/// slash, which is how status reports a nested repository, or a directory on
/// disk. A symlink to a directory is a file to git, so it is not followed.
fn is_directory_entry(root: &Path, relative: &str) -> bool {
    relative.ends_with('/')
        || std::fs::symlink_metadata(root.join(relative)).is_ok_and(|metadata| metadata.is_dir())
}

async fn numstat(
    root: &Path,
    cached: bool,
    enabled: bool,
) -> (HashMap<String, LineCounts>, String, Option<String>) {
    if !enabled {
        return (HashMap::new(), String::new(), None);
    }
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--numstat",
        "-z",
    ];
    if cached {
        args.push("--cached");
    }
    let stage = if cached { "staged" } else { "unstaged" };
    match command(root, &args, MAX_LIST_BYTES)
        .await
        .and_then(require_success)
    {
        Ok(output) => {
            let counts = GitStatusParser::parse_numstat(&output.stdout);
            let notice = output.truncated.then(|| {
                format!(
                    "Line totals are partial: {stage} statistics exceeded the 1 MiB read limit."
                )
            });
            // The raw text feeds the snapshot's revision.
            (counts, output.stdout, notice)
        }
        Err(error) => (
            HashMap::new(),
            String::new(),
            Some(format!(
                "Line totals are partial: {stage} statistics are unavailable. {error}"
            )),
        ),
    }
}

/// Untracked files do not appear in Git's numstat. Count their complete text
/// within both a per-file and per-refresh budget; never follow a final symlink.
fn untracked_counts(
    root: &Path,
    relative: &str,
    remaining: &mut usize,
) -> Result<LineCounts, String> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    let parent = path
        .parent()
        .ok_or("Missing parent directory")?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !parent.starts_with(root) {
        return Err("The file resolves outside the project.".into());
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    let bytes = if metadata.file_type().is_symlink() {
        // Git stores the link destination itself as the blob, never its target.
        let bytes = std::fs::read_link(&path)
            .map_err(|error| error.to_string())?
            .as_os_str()
            .as_bytes()
            .to_vec();
        if bytes.len() > *remaining {
            return Err("Link exceeds the remaining read budget".into());
        }
        *remaining -= bytes.len();
        bytes
    } else {
        if !metadata.is_file() {
            return Err("Not a regular file".into());
        }
        let limit = MAX_NEW_FILE_BYTES.min(*remaining);
        if metadata.len() > limit as u64 {
            return Err("File exceeds the remaining read budget".into());
        }
        let (bytes, grew) = read_regular_file(&path, limit)?;
        *remaining = remaining.saturating_sub(bytes.len());
        if grew {
            return Err("File grew beyond the remaining read budget".into());
        }
        bytes
    };
    // Match Git's binary heuristic over the first 8,000 bytes. Non-UTF-8 text
    // can still contain ordinary newlines and has meaningful line counts.
    let binary = bytes.iter().take(8_000).any(|byte| *byte == 0);
    Ok(LineCounts {
        additions: if binary || bytes.is_empty() {
            0
        } else {
            bytes.iter().filter(|byte| **byte == b'\n').count()
                + usize::from(bytes.last() != Some(&b'\n'))
        },
        deletions: 0,
    })
}

/// Read at most `limit` bytes of a regular file without following a final
/// symlink or blocking on a FIFO. Also reports whether more bytes followed.
fn read_regular_file(path: &Path, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("Not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let more = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, more))
}

/// A truncated NUL stream must not turn its last partial filename into a row.
fn complete_records(text: &str) -> &str {
    text.rfind('\0').map_or("", |last| &text[..=last])
}

/// A truncated diff must not end in half a line.
fn complete_lines(text: &str) -> &str {
    text.rfind('\n').map_or("", |last| &text[..=last])
}

/// Build the diff a comparison shows.
///
/// Tracked changes come from one `git diff`; new files never appear there, so
/// each is read directly under the same symlink and containment rules as the
/// line counts and presented as a new-file section. Every source shares one
/// byte budget.
pub async fn project_diff(
    root: &Path,
    mode: ComparisonMode,
    files: Vec<GitFileStatus>,
    has_commits: bool,
) -> Result<DiffDocument, String> {
    let visible: Vec<&GitFileStatus> = files.iter().filter(|file| mode.includes(file)).collect();
    let mut sections = Vec::new();
    let mut remaining = MAX_DIFF_BYTES;
    let mut truncated = false;
    let mut notices = Vec::new();

    if visible.iter().any(|file| !file.is_untracked()) {
        let base = match mode {
            ComparisonMode::Base if has_commits => Some("HEAD".to_string()),
            // An unborn branch has no HEAD; its base is the empty tree.
            ComparisonMode::Base => Some(empty_tree(root).await?),
            _ => None,
        };
        let mut args = vec![
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--submodule=short",
            // Explicit prefixes override `diff.noprefix` and
            // `diff.mnemonicPrefix`, which the path parser relies on.
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ];
        if mode == ComparisonMode::Staged {
            args.push("--cached");
        }
        if let Some(base) = &base {
            args.push(base);
        }
        let output = require_success(command(root, &args, remaining).await?)?;
        let text = if output.truncated {
            truncated = true;
            complete_lines(&output.stdout)
        } else {
            &output.stdout
        };
        remaining = remaining.saturating_sub(text.len());
        sections.extend(git::parse_unified_diff(text));
    }

    if mode != ComparisonMode::Staged {
        let new_files: Vec<&str> = visible
            .iter()
            .filter(|file| file.is_untracked())
            .map(|file| file.path.as_str())
            .collect();
        for (shown, path) in new_files.iter().enumerate() {
            if shown >= MAX_NEW_FILES_IN_DIFF || remaining == 0 {
                notices.push(format!(
                    "{} more new file(s) are listed but not shown.",
                    new_files.len() - shown
                ));
                break;
            }
            let (section, cut) = untracked_section(root, path, &mut remaining);
            truncated |= cut;
            sections.push(section);
        }
    }

    sections.sort_by(|a, b| a.path.cmp(&b.path));
    let by_path: HashMap<&str, &GitFileStatus> = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let statuses = sections
        .iter()
        .map(|section| {
            by_path
                .get(section.path.as_str())
                .map(|file| mode.status(file))
                .unwrap_or_default()
        })
        .collect();
    if truncated {
        notices.insert(
            0,
            "Diff truncated to stay within 4 MiB. Open a file in its editor for the complete contents."
                .into(),
        );
    }
    Ok(DiffDocument {
        sections,
        statuses,
        notice: (!notices.is_empty()).then(|| notices.join(" ")),
    })
}

/// The empty tree in this repository's object format.
async fn empty_tree(root: &Path) -> Result<String, String> {
    let output = require_success(
        command(
            root,
            &["hash-object", "-t", "tree", "/dev/null"],
            MAX_LIST_BYTES,
        )
        .await?,
    )?;
    Ok(output.stdout.trim().to_string())
}

/// A new file as a diff section, read under the same rules as its line count.
/// Returns whether the file was cut short.
fn untracked_section(root: &Path, relative: &str, remaining: &mut usize) -> (DiffSection, bool) {
    let meta = |lines: &[&str]| {
        git::diff::parse_section(Some(relative.to_string()), lines.iter().copied())
            .unwrap_or_else(|| DiffSection::from_lines(relative, Vec::new()))
    };
    if let Err(error) = validate_relative_path(relative) {
        return (meta(&["new file", &error]), false);
    }
    if is_directory_entry(root, relative) {
        return (
            meta(&["Directory, usually a nested repository. Its files are not shown here."]),
            false,
        );
    }
    let path = root.join(relative);
    let contained = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(root.canonicalize().ok())
        .is_some_and(|(parent, root)| parent.starts_with(root));
    if !contained {
        return (
            meta(&[
                "new file",
                "This file resolves outside the project and is not shown.",
            ]),
            false,
        );
    }
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return (meta(&["new file", &format!("Cannot read: {error}")]), false),
    };
    if metadata.file_type().is_symlink() {
        // Git records the link's destination as its content, never the target.
        let destination = std::fs::read_link(&path)
            .map(|destination| destination.to_string_lossy().into_owned())
            .unwrap_or_default();
        *remaining = remaining.saturating_sub(destination.len());
        let addition = format!("+{destination}");
        return (
            meta(&["new file mode 120000", "@@ -0,0 +1 @@", &addition]),
            false,
        );
    }
    if !metadata.is_file() {
        return (
            meta(&["new file", "Not a regular file; nothing to show."]),
            false,
        );
    }
    let mode = if metadata.permissions().mode() & 0o111 != 0 {
        "new file mode 100755"
    } else {
        "new file mode 100644"
    };
    let limit = MAX_NEW_FILE_BYTES.min(*remaining);
    let (mut bytes, more) = match read_regular_file(&path, limit) {
        Ok(read) => read,
        Err(error) => return (meta(&[mode, &format!("Cannot read: {error}")]), false),
    };
    *remaining = remaining.saturating_sub(bytes.len());
    if bytes.iter().take(8_000).any(|byte| *byte == 0) {
        return (
            meta(&[mode, "Binary file; text preview is unavailable."]),
            false,
        );
    }
    // A byte ceiling may split the final UTF-8 character, but arbitrary invalid
    // bytes should not turn a binary file into replacement-character prose.
    if more {
        while std::str::from_utf8(&bytes)
            .err()
            .is_some_and(|error| error.error_len().is_none())
        {
            bytes.pop();
        }
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return (
            meta(&[mode, "Not UTF-8 text. Open it in its usual editor."]),
            false,
        );
    };
    if text.is_empty() {
        return (meta(&[mode, "Empty file."]), more);
    }
    let lines: Vec<&str> = git::diff::diff_lines(&text).collect();
    let hunk = format!("@@ -0,0 +1,{} @@", lines.len());
    let mut raw = vec![mode.to_string(), hunk];
    raw.extend(lines.iter().map(|line| format!("+{line}")));
    if more {
        raw.push(format!(
            "Showing the first {} KiB of this file.",
            MAX_NEW_FILE_BYTES / 1024
        ));
    }
    let section =
        git::diff::parse_section(Some(relative.to_string()), raw.iter().map(String::as_str))
            .unwrap_or_else(|| DiffSection::from_lines(relative, Vec::new()));
    (section, more)
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("This file path is outside the project.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::git::DiffLineKind;

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new(git::GIT)
            .args(args)
            .current_dir(root)
            .envs(git::hardened_environment())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "FlowX test"]);
        temp
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn texts(section: &DiffSection) -> Vec<(DiffLineKind, &str)> {
        section
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }

    #[test]
    fn the_shared_worker_runs_jobs_called_from_a_small_dispatch_stack() {
        let temp = repository();
        std::fs::write(temp.path().join("small.txt"), "small-stack preview\n").unwrap();
        let root = temp.path().to_path_buf();
        let caller = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let for_snapshot = root.clone();
                let current =
                    spawn_git(move || async move { changes_snapshot(&for_snapshot).await })
                        .unwrap()
                        .blocking_recv()
                        .unwrap()
                        .unwrap();
                let changes = current.repository.unwrap().files;
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].path, "small.txt");
                let diff = spawn_git(move || async move {
                    project_diff(&root, ComparisonMode::Unstaged, changes, false).await
                })
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
                assert_eq!(diff.sections[0].path, "small.txt");
                assert_eq!(
                    texts(&diff.sections[0])[2],
                    (DiffLineKind::Addition, "+small-stack preview")
                );
            })
            .unwrap();
        caller.join().unwrap();
    }

    #[test]
    fn a_real_repository_lists_changes_and_separates_staged_unstaged_and_base() {
        let temp = repository();
        let file = temp.path().join("file with spaces.txt");
        std::fs::write(&file, "base\n").unwrap();
        std::fs::write(temp.path().join("unchanged.txt"), "untouched\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        std::fs::write(&file, "staged\n").unwrap();
        git(temp.path(), &["add", "."]);
        std::fs::write(&file, "working\n").unwrap();
        std::fs::write(temp.path().join("new.txt"), "new content\n").unwrap();
        let before_index = std::fs::read(temp.path().join(".git/index")).unwrap();
        let snapshot = block_on(changes_snapshot(temp.path())).unwrap();
        assert!(snapshot.notice.is_none(), "{:?}", snapshot.notice);
        let info = snapshot.repository.unwrap();
        assert_eq!(info.files.len(), 2);
        assert_eq!((info.additions, info.deletions), (3, 2));
        assert!(info.files.iter().all(|file| file.path != "unchanged.txt"));
        let change = info
            .files
            .iter()
            .find(|file| file.path == "file with spaces.txt")
            .unwrap()
            .clone();
        assert_eq!((change.staged_additions, change.staged_deletions), (1, 1));
        assert_eq!(
            (change.unstaged_additions, change.unstaged_deletions),
            (1, 1)
        );
        let diff =
            |mode| block_on(project_diff(&snapshot.root, mode, info.files.clone(), true)).unwrap();
        let lines = |document: &DiffDocument, path: &str| -> Vec<String> {
            document
                .sections
                .iter()
                .find(|section| section.path == path)
                .map(|section| section.lines.iter().map(|line| line.text.clone()).collect())
                .unwrap_or_default()
        };
        let unstaged = diff(ComparisonMode::Unstaged);
        assert_eq!(
            unstaged
                .sections
                .iter()
                .map(|section| section.path.as_str())
                .collect::<Vec<_>>(),
            ["file with spaces.txt", "new.txt"]
        );
        assert!(lines(&unstaged, "file with spaces.txt").contains(&"-staged".into()));
        assert!(lines(&unstaged, "file with spaces.txt").contains(&"+working".into()));
        assert_eq!(unstaged.statuses, ["M", "?"]);
        let staged = diff(ComparisonMode::Staged);
        assert_eq!(staged.sections.len(), 1, "a new file is never staged");
        assert!(lines(&staged, "file with spaces.txt").contains(&"-base".into()));
        assert!(lines(&staged, "file with spaces.txt").contains(&"+staged".into()));
        let base = diff(ComparisonMode::Base);
        assert!(lines(&base, "file with spaces.txt").contains(&"-base".into()));
        assert!(lines(&base, "file with spaces.txt").contains(&"+working".into()));
        assert!(lines(&base, "new.txt").contains(&"+new content".into()));
        assert_eq!(
            std::fs::read(temp.path().join(".git/index")).unwrap(),
            before_index,
            "reads must not refresh or mutate the index"
        );
    }

    #[test]
    fn diff_content_resembling_headers_is_classified_by_hunk_state() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("query.sql"), "-- comment\nselect 1;\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        std::fs::write(root.join("query.sql"), "++i;\nselect 1;\n 3 changes\n").unwrap();
        let snapshot = block_on(changes_snapshot(root)).unwrap();
        let files = snapshot.repository.unwrap().files;
        let document = block_on(project_diff(
            &snapshot.root,
            ComparisonMode::Unstaged,
            files,
            true,
        ))
        .unwrap();
        let kinds: Vec<_> = texts(&document.sections[0])
            .into_iter()
            .filter(|(kind, _)| *kind != DiffLineKind::Hunk)
            .collect();
        assert_eq!(
            kinds,
            [
                (DiffLineKind::Deletion, "--- comment"),
                (DiffLineKind::Addition, "+++i;"),
                (DiffLineKind::Context, " select 1;"),
                (DiffLineKind::Addition, "+ 3 changes"),
            ]
        );
    }

    #[test]
    fn real_line_totals_include_deletions_new_files_renames_and_binary_file_status() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("delete.txt"), "one\ntwo\nthree\n").unwrap();
        let original = (0..12)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(root.join("old\tname.txt"), &original).unwrap();
        std::fs::write(root.join("binary"), b"before\0bytes").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        git(root, &["mv", "old\tname.txt", "new\nname.txt"]);
        std::fs::write(
            root.join("new\nname.txt"),
            original.replace("line 0\n", "changed\n"),
        )
        .unwrap();
        std::fs::write(root.join("staged.txt"), "one\ntwo\n").unwrap();
        git(root, &["add", "."]);
        std::fs::remove_file(root.join("delete.txt")).unwrap();
        std::fs::write(root.join("new.txt"), "one\nlast line without a newline").unwrap();
        std::fs::write(root.join("binary"), b"after\0bytes").unwrap();
        let before_index = std::fs::read(root.join(".git/index")).unwrap();
        let snapshot = block_on(changes_snapshot(root)).unwrap();
        assert!(snapshot.notice.is_none(), "{:?}", snapshot.notice);
        let info = snapshot.repository.unwrap();
        assert_eq!(info.status_file_count, 5);
        assert_eq!((info.additions, info.deletions), (5, 4));
        let renamed = info
            .files
            .iter()
            .find(|file| file.path == "new\nname.txt")
            .unwrap();
        assert_eq!(renamed.staged_status, "R");
        assert_eq!(renamed.original_path.as_deref(), Some("old\tname.txt"));
        assert_eq!((renamed.staged_additions, renamed.staged_deletions), (1, 1));
        let binary = info
            .files
            .iter()
            .find(|file| file.path == "binary")
            .unwrap();
        assert_eq!((binary.additions(), binary.deletions()), (0, 0));
        assert!(binary.has_unstaged_changes());
        let staged = block_on(project_diff(
            &snapshot.root,
            ComparisonMode::Staged,
            info.files.clone(),
            true,
        ))
        .unwrap();
        assert!(
            staged
                .sections
                .iter()
                .any(|section| section.path == "new\nname.txt"),
            "a quoted rename destination resolves: {:?}",
            staged.sections.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            before_index
        );
    }

    #[test]
    fn incomplete_counts_are_visible_and_symlinks_never_count_their_targets() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("large.txt"), "line\n".repeat(MAX_NEW_FILE_BYTES)).unwrap();
        std::fs::write(root.join("small.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("binary"), b"new\0binary").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside\n".repeat(100)).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let snapshot = block_on(changes_snapshot(root)).unwrap();
        assert!(snapshot.notice.unwrap().contains("Line totals are partial"));
        let info = snapshot.repository.unwrap();
        assert_eq!(info.status_file_count, 4);
        assert_eq!((info.additions, info.deletions), (3, 0));
        let mut budget = 1;
        assert!(untracked_counts(&root.canonicalize().unwrap(), "small.txt", &mut budget).is_err());
        assert_eq!(budget, 1, "oversized files are not opened");
        std::os::unix::fs::symlink(outside.path().parent().unwrap(), root.join("external"))
            .unwrap();
        let through_link = format!(
            "external/{}",
            outside.path().file_name().unwrap().to_str().unwrap()
        );
        assert!(
            untracked_counts(&root.canonicalize().unwrap(), &through_link, &mut budget).is_err()
        );
    }

    #[test]
    fn a_nested_repository_does_not_make_line_totals_partial() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("top.txt"), "top\n").unwrap();
        let nested = root.join("vendor/lib");
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        std::fs::write(nested.join("inner.txt"), "inner\n").unwrap();
        let snapshot = block_on(changes_snapshot(root)).unwrap();
        let info = snapshot.repository.as_ref().unwrap();
        assert!(
            info.files.iter().any(|file| file.path == "vendor/lib/"),
            "{:?}",
            info.files
        );
        assert!(snapshot.notice.is_none(), "{:?}", snapshot.notice);
        assert_eq!(info.additions, 1);
        let document = block_on(project_diff(
            &snapshot.root,
            ComparisonMode::Unstaged,
            info.files.clone(),
            false,
        ))
        .unwrap();
        let nested = document
            .sections
            .iter()
            .find(|section| section.path == "vendor/lib/")
            .unwrap();
        assert_eq!(nested.lines[0].kind, DiffLineKind::Meta);
        assert!(document.notice.is_none());
    }

    #[test]
    fn the_revision_follows_the_working_tree_and_index() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let revision = || {
            block_on(changes_snapshot(root))
                .unwrap()
                .repository
                .unwrap()
                .revision
        };
        let first = revision();
        assert_eq!(first, revision(), "an unchanged tree keeps its revision");
        assert_ne!(first, GitStatusParser::revision(1, false), "not a constant");
        // Same size, same status and same numstat: only metadata differs.
        std::fs::write(root.join("a.txt"), "six\n").unwrap();
        let rewritten = revision();
        assert_ne!(first, rewritten, "a same-size rewrite must change it");
        git(root, &["add", "a.txt"]);
        assert_ne!(rewritten, revision(), "staging must change it");
    }

    #[test]
    fn adding_or_removing_a_remote_is_a_new_snapshot() {
        // Push is offered only with a remote, so a snapshot that read the same
        // across `git remote add` would leave the button as it was.
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "initial"]);
        let read = || block_on(changes_snapshot(root)).unwrap();
        let without = read();
        assert!(!without.repository.as_ref().unwrap().can_push());
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let with = read();
        assert!(!with.same_as(&without), "adding a remote read as no change");
        assert!(with.repository.as_ref().unwrap().can_push());
        git(root, &["remote", "remove", "origin"]);
        let removed = read();
        assert!(
            !removed.same_as(&with),
            "removing a remote read as no change"
        );
        assert!(!removed.repository.as_ref().unwrap().can_push());
    }

    #[tokio::test]
    async fn a_failed_numstat_reports_incomplete_counts() {
        // A repository can disappear between the status and numstat reads.
        let temp = repository();
        std::fs::remove_dir_all(temp.path().join(".git")).unwrap();
        let (counts, _, notice) = numstat(temp.path(), true, true).await;
        assert!(counts.is_empty());
        assert!(notice.unwrap().contains("statistics are unavailable"));
    }

    #[test]
    fn an_unborn_repository_diffs_against_the_empty_tree_while_a_plain_folder_has_no_repository() {
        let temp = repository();
        std::fs::write(temp.path().join("new.txt"), "hello\n").unwrap();
        std::fs::write(temp.path().join("added.txt"), "staged\n").unwrap();
        git(temp.path(), &["add", "added.txt"]);
        let current = block_on(changes_snapshot(temp.path())).unwrap();
        let info = current.repository.unwrap();
        assert!(!info.has_commits);
        for mode in ComparisonMode::ALL {
            let document =
                block_on(project_diff(&current.root, mode, info.files.clone(), false)).unwrap();
            let paths: Vec<_> = document.sections.iter().map(|s| s.path.as_str()).collect();
            match mode {
                ComparisonMode::Unstaged => assert_eq!(paths, ["new.txt"]),
                ComparisonMode::Staged => assert_eq!(paths, ["added.txt"]),
                ComparisonMode::Base => assert_eq!(paths, ["added.txt", "new.txt"]),
            }
        }
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("readme.txt"), "plain text").unwrap();
        let current = block_on(changes_snapshot(plain.path())).unwrap();
        assert!(current.repository.is_none());
        assert!(current.notice.is_none());
        assert_eq!(current.root, plain.path().canonicalize().unwrap());
    }

    #[test]
    fn binary_large_and_external_new_files_have_truthful_bounded_sections() {
        let temp = repository();
        let root = temp.path().canonicalize().unwrap();
        let section = |path: &str| {
            let mut remaining = MAX_DIFF_BYTES;
            untracked_section(&root, path, &mut remaining)
        };
        std::fs::write(root.join("binary"), b"a\0b").unwrap();
        assert!(texts(&section("binary").0)[1].1.contains("Binary file"));
        std::fs::write(root.join("large"), "line\n".repeat(MAX_NEW_FILE_BYTES)).unwrap();
        let (large, cut) = section("large");
        assert!(cut);
        // Whole lines plus the one the byte ceiling cut.
        assert!(large.additions <= MAX_NEW_FILE_BYTES / 5 + 1);
        assert!(
            large
                .lines
                .last()
                .unwrap()
                .text
                .starts_with("Showing the first")
        );
        assert!(
            texts(&section("../outside").0)[1]
                .1
                .contains("outside the project")
        );
        std::os::unix::fs::symlink("/etc/passwd", root.join("outside")).unwrap();
        assert_eq!(
            texts(&section("outside").0),
            [
                (DiffLineKind::Meta, "new file mode 120000"),
                (DiffLineKind::Hunk, "@@ -0,0 +1 @@"),
                (DiffLineKind::Addition, "+/etc/passwd"),
            ],
            "a link shows its destination, never the target's contents"
        );
        let mut exhausted = 0;
        let (_, _) = untracked_section(&root, "large", &mut exhausted);
        assert_eq!(exhausted, 0);
    }

    #[test]
    fn nul_records_and_diff_lines_never_end_partially() {
        assert_eq!(
            complete_records("one\0file\nwith newline\0partial"),
            "one\0file\nwith newline\0"
        );
        assert_eq!(complete_records("partial"), "");
        assert_eq!(complete_lines("+a\n+b\n+parti"), "+a\n+b\n");
    }
}
