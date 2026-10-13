//! Bounded, read-only Git status and changed-file previews.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use fx_core::git::{self, CommandOutput, CommandSpec, GitStatusParser, LineCounts};
use fx_core::policy::diff_budget::DiffBudget;
use fx_core::{GitFileStatus, GitRepositoryInfo};

pub const MAX_FILES: usize = 2_000;
pub const MAX_PREVIEW_BYTES: usize = 128 * 1024;
pub const MAX_PREVIEW_LINES: usize = 1_000;
const MAX_LIST_BYTES: usize = 1024 * 1024;
const MAX_UNTRACKED_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// macOS dispatch workers have small stacks. Tokio's nested Git read futures
/// can exceed those stacks in debug builds even with heap-backed read buffers.
/// Construct and poll them entirely on a worker with a known stack size; GPUI
/// only holds the receiver and never blocks on filesystem or subprocess I/O.
pub fn spawn_read<T: Send + 'static>(
    read: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<tokio::sync::oneshot::Receiver<Result<T, String>>, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("flowx-inspector".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let _ = sender.send(read());
        })
        .map_err(|error| format!("Cannot start the inspector reader: {error}"))?;
    Ok(receiver)
}

pub async fn finish_read<T>(
    job: Result<tokio::sync::oneshot::Receiver<Result<T, String>>, String>,
) -> Result<T, String> {
    job?.await.map_err(|_| {
        "The inspector reader stopped unexpectedly. Use Refresh to retry.".to_string()
    })?
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub root: PathBuf,
    pub repository: Option<GitRepositoryInfo>,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Change(GitFileStatus),
}

impl Selection {
    pub fn path(&self) -> &str {
        match self {
            Self::Change(status) => &status.path,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    Header,
}

#[derive(Clone, Debug)]
pub struct PreviewLine {
    pub text: String,
    pub kind: LineKind,
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub text: String,
    pub lines: Vec<PreviewLine>,
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
    let probe = command(&root, &["rev-parse", "--show-toplevel"], MAX_LIST_BYTES).await?;
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
    let root = PathBuf::from(probe.stdout.trim_end_matches('\n'));
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
    let mut status = GitStatusParser::parse_status(complete_records(&status_output.stdout));
    let truncated = status_output.truncated || status.files.len() > MAX_FILES;
    status.files.truncate(MAX_FILES);
    let mut notices = Vec::new();
    if truncated {
        notices.push(format!(
            "Changed-file lists and line totals are partial: limited to {MAX_FILES} entries and 1 MiB."
        ));
    }
    let ((mut unstaged, unstaged_notice), (staged, staged_notice)) = tokio::join!(
        numstat(&root, false, status.needs_unstaged_stats),
        numstat(&root, true, status.needs_staged_stats),
    );
    notices.extend(unstaged_notice);
    notices.extend(staged_notice);
    let mut remaining = MAX_UNTRACKED_TOTAL_BYTES;
    let mut uncounted = 0;
    for file in &status.files {
        if file.staged_status != "?" || file.unstaged_status != "?" {
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
    let info = GitStatusParser::make_info(&status, &unstaged, &staged, false, 1);
    Ok(Snapshot {
        root,
        repository: Some(info),
        notice: (!notices.is_empty()).then(|| notices.join(" ")),
    })
}

async fn numstat(
    root: &Path,
    cached: bool,
    enabled: bool,
) -> (HashMap<String, LineCounts>, Option<String>) {
    if !enabled {
        return (HashMap::new(), None);
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
        Ok(output) => (
            GitStatusParser::parse_numstat(&output.stdout),
            output.truncated.then(|| {
                format!(
                    "Line totals are partial: {stage} statistics exceeded the 1 MiB read limit."
                )
            }),
        ),
        Err(error) => (
            HashMap::new(),
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
        let limit = MAX_PREVIEW_BYTES.min(*remaining);
        if metadata.len() > limit as u64 {
            return Err("File exceeds the remaining read budget".into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
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
        *remaining = remaining.saturating_sub(bytes.len());
        if bytes.len() > limit {
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

/// A truncated NUL stream must not turn its last partial filename into a row.
fn complete_records(text: &str) -> &str {
    text.rfind('\0').map_or("", |last| &text[..=last])
}

pub async fn preview(root: &Path, selected: &Selection) -> Result<Preview, String> {
    validate_relative_path(selected.path())?;
    match selected {
        Selection::Change(status) if status.is_untracked() => {
            read_untracked_file(root, &status.path)
        }
        Selection::Change(status) => {
            let mut budget = DiffBudget::new(MAX_PREVIEW_BYTES);
            for (enabled, cached, heading) in [
                (status.has_staged_changes(), true, "Staged changes"),
                (status.has_unstaged_changes(), false, "Unstaged changes"),
            ] {
                if !enabled {
                    continue;
                }
                let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
                if cached {
                    args.push("--cached");
                }
                args.extend(["--", status.path.as_str()]);
                let output = require_success(command(root, &args, MAX_PREVIEW_BYTES / 2).await?)?;
                let content = if output.stdout.is_empty() {
                    "No diff available for this file.\n"
                } else {
                    &output.stdout
                };
                if !budget.append(&format!("{heading}\n{content}"), output.truncated) {
                    break;
                }
            }
            Ok(make_preview(budget.output(), true, budget.was_truncated()))
        }
    }
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

fn read_untracked_file(root: &Path, relative: &str) -> Result<Preview, String> {
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("Cannot read {relative}: {error}"))?;
    if metadata.file_type().is_symlink() {
        let destination = std::fs::read_link(&path).map_err(|error| error.to_string())?;
        return Ok(make_preview(
            format!("Symbolic link → {}", destination.display()),
            false,
            false,
        ));
    }
    if !metadata.is_file() {
        return Err("This entry is not a regular file and cannot be previewed.".into());
    }
    let resolved = path.canonicalize().map_err(|error| error.to_string())?;
    let canonical_root = root.canonicalize().map_err(|error| error.to_string())?;
    if !resolved.starts_with(&canonical_root) {
        return Err("This file resolves outside the project and cannot be previewed.".into());
    }
    let file = std::fs::File::open(&resolved).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take((MAX_PREVIEW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let truncated = bytes.len() > MAX_PREVIEW_BYTES;
    bytes.truncate(MAX_PREVIEW_BYTES);
    if bytes.contains(&0) {
        return Ok(make_preview(
            "Binary file — text preview is unavailable.".into(),
            false,
            false,
        ));
    }
    // A byte ceiling may split the final UTF-8 character, but arbitrary invalid
    // bytes should not turn a binary file into replacement-character prose.
    if truncated {
        while std::str::from_utf8(&bytes)
            .err()
            .is_some_and(|error| error.error_len().is_none())
        {
            bytes.pop();
        }
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| "This file is not UTF-8 text. Open it in its usual editor.".to_string())?;
    let text = format!(
        "Untracked file\n{}",
        text.lines()
            .map(|line| format!("+{line}\n"))
            .collect::<String>()
    );
    Ok(make_preview(text, true, truncated))
}

fn make_preview(mut text: String, diff: bool, truncated: bool) -> Preview {
    let mut shortened = truncated;
    if text.len() > MAX_PREVIEW_BYTES {
        let mut end = MAX_PREVIEW_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        shortened = true;
    }
    let lines = text
        .lines()
        .take(MAX_PREVIEW_LINES)
        .map(|line| {
            let kind = if diff
                && (line.starts_with("+++")
                    || line.starts_with("---")
                    || line.starts_with("@@")
                    || line.starts_with("diff ")
                    || line.ends_with(" changes"))
            {
                LineKind::Header
            } else if diff && line.starts_with('+') {
                LineKind::Added
            } else if diff && line.starts_with('-') {
                LineKind::Removed
            } else {
                LineKind::Context
            };
            let text = if line.chars().count() > 2_000 {
                shortened = true;
                format!("{}…", line.chars().take(2_000).collect::<String>())
            } else {
                line.to_string()
            };
            PreviewLine { text, kind }
        })
        .collect::<Vec<_>>();
    shortened |= text.lines().count() > MAX_PREVIEW_LINES;
    Preview {
        text,
        lines,
        notice: shortened.then(|| "Preview truncated: showing up to 128 KiB, 1,000 lines and 2,000 characters per line. Open the file for the complete contents.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn untracked(path: &str) -> Selection {
        Selection::Change(GitFileStatus::new(path, "?", "?"))
    }

    #[test]
    fn the_background_reader_works_when_called_from_a_small_dispatch_stack() {
        let temp = repository();
        std::fs::write(temp.path().join("small.txt"), "small-stack preview\n").unwrap();
        let root = temp.path().to_path_buf();
        let caller = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                let for_snapshot = root.clone();
                let current = spawn_read(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(changes_snapshot(&for_snapshot))
                })
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
                let changes = current.repository.unwrap().files;
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].path, "small.txt");
                let shown = spawn_read(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(preview(&root, &untracked("small.txt")))
                })
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
                assert_eq!(shown.text, "Untracked file\n+small-stack preview\n");
            })
            .unwrap();
        caller.join().unwrap();
    }

    #[tokio::test]
    async fn a_real_repository_lists_changes_and_separates_staged_and_unstaged_content() {
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
        let snapshot = changes_snapshot(temp.path()).await.unwrap();
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
        let shown = preview(&snapshot.root, &Selection::Change(change))
            .await
            .unwrap();
        assert!(shown.text.contains("Staged changes\n"));
        assert!(shown.text.contains("Unstaged changes\n"));
        assert!(shown.text.contains("+staged"));
        assert!(shown.text.contains("+working"));
        assert_eq!(
            std::fs::read(temp.path().join(".git/index")).unwrap(),
            before_index,
            "reads must not refresh or mutate the index"
        );
    }

    #[tokio::test]
    async fn real_line_totals_include_deletions_new_files_renames_and_binary_file_status() {
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
        let snapshot = changes_snapshot(root).await.unwrap();
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
        assert_eq!((renamed.staged_additions, renamed.staged_deletions), (1, 1));
        let binary = info
            .files
            .iter()
            .find(|file| file.path == "binary")
            .unwrap();
        assert_eq!((binary.additions(), binary.deletions()), (0, 0));
        assert!(binary.has_unstaged_changes());
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            before_index
        );
    }

    #[tokio::test]
    async fn incomplete_counts_are_visible_and_symlinks_never_count_their_targets() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("large.txt"), "line\n".repeat(MAX_PREVIEW_BYTES)).unwrap();
        std::fs::write(root.join("small.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("binary"), b"new\0binary").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside\n".repeat(100)).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let snapshot = changes_snapshot(root).await.unwrap();
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

    #[tokio::test]
    async fn a_failed_numstat_reports_incomplete_counts() {
        // A repository can disappear between the status and numstat reads.
        let temp = repository();
        std::fs::remove_dir_all(temp.path().join(".git")).unwrap();
        let (counts, notice) = numstat(temp.path(), true, true).await;
        assert!(counts.is_empty());
        assert!(notice.unwrap().contains("statistics are unavailable"));
    }

    #[tokio::test]
    async fn an_unborn_repository_shows_untracked_changes_while_a_plain_folder_has_no_repository() {
        let temp = repository();
        std::fs::write(temp.path().join("new.txt"), "hello\n").unwrap();
        let current = changes_snapshot(temp.path()).await.unwrap();
        assert!(!current.repository.as_ref().unwrap().has_commits);
        let change = current.repository.unwrap().files.remove(0);
        assert!(
            preview(&current.root, &Selection::Change(change))
                .await
                .unwrap()
                .text
                .contains("+hello")
        );
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("readme.txt"), "plain text").unwrap();
        let current = changes_snapshot(plain.path()).await.unwrap();
        assert!(current.repository.is_none());
        assert!(current.notice.is_none());
        assert_eq!(current.root, plain.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn binary_large_and_external_files_have_truthful_bounded_previews() {
        let temp = repository();
        std::fs::write(temp.path().join("binary"), b"a\0b").unwrap();
        assert!(
            preview(temp.path(), &untracked("binary"))
                .await
                .unwrap()
                .text
                .contains("Binary file")
        );
        std::fs::write(
            temp.path().join("large"),
            "line\n".repeat(MAX_PREVIEW_BYTES),
        )
        .unwrap();
        let large = preview(temp.path(), &untracked("large")).await.unwrap();
        assert_eq!(large.lines.len(), MAX_PREVIEW_LINES);
        assert!(large.notice.is_some());
        assert!(large.text.len() <= MAX_PREVIEW_BYTES);
        assert!(
            preview(temp.path(), &untracked("../outside"))
                .await
                .is_err()
        );
        std::os::unix::fs::symlink("/etc/passwd", temp.path().join("outside")).unwrap();
        let link = preview(temp.path(), &untracked("outside")).await.unwrap();
        assert_eq!(link.text, "Symbolic link → /etc/passwd");
    }

    #[test]
    fn nul_records_preserve_unusual_names_without_partial_truncated_rows() {
        assert_eq!(
            complete_records("one\0file\nwith newline\0partial"),
            "one\0file\nwith newline\0"
        );
        assert_eq!(complete_records("partial"), "");
    }
}
