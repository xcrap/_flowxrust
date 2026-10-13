//! Bounded, read-only repository and file reads for the inspector.

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use fx_core::git::{self, CommandOutput, CommandSpec, GitStatusParser};
use fx_core::policy::diff_budget::DiffBudget;
use fx_core::{GitFileStatus, GitRepositoryInfo};

pub const MAX_FILES: usize = 2_000;
pub const MAX_PREVIEW_BYTES: usize = 128 * 1024;
pub const MAX_PREVIEW_LINES: usize = 1_000;
const MAX_LIST_BYTES: usize = 1024 * 1024;

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
    pub files: Vec<String>,
    pub notice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    File(String),
    Change(GitFileStatus),
}

impl Selection {
    pub fn path(&self) -> &str {
        match self {
            Self::File(path) => path,
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

/// Load a repository snapshot, or a bounded directory listing for a folder
/// without Git. Errors remain visible instead of resembling a clean tree.
pub async fn snapshot(root: &Path) -> Result<Snapshot, String> {
    read_snapshot(root, true).await
}

/// The inline conversation surface needs only Git status, not a recursive file
/// listing. Keeping that separate avoids walking non-repository project folders
/// and enumerating every tracked file on its background refresh.
pub async fn changes_snapshot(root: &Path) -> Result<Snapshot, String> {
    read_snapshot(root, false).await
}

async fn read_snapshot(root: &Path, include_files: bool) -> Result<Snapshot, String> {
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
        let (files, truncated) = if include_files {
            directory_files(&root)?
        } else {
            (Vec::new(), false)
        };
        return Ok(Snapshot {
            root,
            repository: None,
            files,
            notice: truncated.then(|| format!("Showing the first {MAX_FILES} files.")),
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
    let list_output = if include_files {
        Some(require_success(
            command(
                &root,
                &[
                    "ls-files",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                ],
                MAX_LIST_BYTES,
            )
            .await?,
        )?)
    } else {
        None
    };
    let mut status = GitStatusParser::parse_status(complete_records(&status_output.stdout));
    let truncated = status_output.truncated
        || list_output.as_ref().is_some_and(|list| list.truncated)
        || status.files.len() > MAX_FILES;
    status.files.truncate(MAX_FILES);
    let info = GitStatusParser::make_info(&status, &HashMap::new(), &HashMap::new(), false, 1);
    let mut files: Vec<_> = complete_records(
        list_output
            .as_ref()
            .map(|list| list.stdout.as_str())
            .unwrap_or_default(),
    )
    .split_terminator('\0')
    .filter(|path| !path.is_empty())
    .map(String::from)
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect();
    let truncated = truncated || files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    Ok(Snapshot {
        root,
        repository: Some(info),
        files,
        notice: truncated
            .then(|| format!("File lists are limited to {MAX_FILES} entries and 1 MiB.")),
    })
}

/// A truncated NUL stream must not turn its last partial filename into a row.
fn complete_records(text: &str) -> &str {
    text.rfind('\0').map_or("", |last| &text[..=last])
}

fn directory_files(root: &Path) -> Result<(Vec<String>, bool), String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    let mut inspected = 0usize;
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Cannot list {}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("Cannot read a directory entry: {error}"))?;
            if entry.file_name() == ".git" {
                continue;
            }
            inspected += 1;
            if files.len() >= MAX_FILES || inspected > MAX_FILES * 4 {
                files.sort();
                return Ok((files, true));
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() || kind.is_symlink() {
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    files.sort();
    Ok((files, false))
}

pub async fn preview(root: &Path, selected: &Selection) -> Result<Preview, String> {
    validate_relative_path(selected.path())?;
    match selected {
        Selection::File(path) => read_file(root, path, false),
        Selection::Change(status) if status.is_untracked() => read_file(root, &status.path, true),
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

fn read_file(root: &Path, relative: &str, as_added: bool) -> Result<Preview, String> {
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
    let text = if as_added {
        format!(
            "Untracked file\n{}",
            text.lines()
                .map(|line| format!("+{line}\n"))
                .collect::<String>()
        )
    } else {
        text
    };
    Ok(make_preview(text, as_added, truncated))
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
                        .block_on(snapshot(&for_snapshot))
                })
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
                assert!(current.files.contains(&"small.txt".to_string()));
                let shown = spawn_read(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(preview(&root, &Selection::File("small.txt".into())))
                })
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
                assert_eq!(shown.text, "small-stack preview\n");
            })
            .unwrap();
        caller.join().unwrap();
    }

    #[tokio::test]
    async fn a_real_repository_lists_changes_and_separates_staged_and_unstaged_content() {
        let temp = repository();
        let file = temp.path().join("file with spaces.txt");
        std::fs::write(&file, "base\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        std::fs::write(&file, "staged\n").unwrap();
        git(temp.path(), &["add", "."]);
        std::fs::write(&file, "working\n").unwrap();
        std::fs::write(temp.path().join("new.txt"), "new content\n").unwrap();
        let before_index = std::fs::read(temp.path().join(".git/index")).unwrap();
        let snapshot = snapshot(temp.path()).await.unwrap();
        assert_eq!(snapshot.files, ["file with spaces.txt", "new.txt"]);
        let info = snapshot.repository.unwrap();
        assert_eq!(info.files.len(), 2);
        let change = info
            .files
            .iter()
            .find(|file| file.path == "file with spaces.txt")
            .unwrap()
            .clone();
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
    async fn an_unborn_repository_and_plain_folder_both_offer_file_previews() {
        let temp = repository();
        std::fs::write(temp.path().join("new.txt"), "hello\n").unwrap();
        let current = snapshot(temp.path()).await.unwrap();
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
        let current = snapshot(plain.path()).await.unwrap();
        assert!(current.repository.is_none());
        assert_eq!(current.files, ["readme.txt"]);
        assert_eq!(
            preview(&current.root, &Selection::File("readme.txt".into()))
                .await
                .unwrap()
                .text,
            "plain text"
        );
    }

    #[tokio::test]
    async fn binary_large_and_external_files_have_truthful_bounded_previews() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("binary"), b"a\0b").unwrap();
        assert!(
            preview(temp.path(), &Selection::File("binary".into()))
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
        let large = preview(temp.path(), &Selection::File("large".into()))
            .await
            .unwrap();
        assert_eq!(large.lines.len(), MAX_PREVIEW_LINES);
        assert!(large.notice.is_some());
        assert!(large.text.len() <= MAX_PREVIEW_BYTES);
        assert!(
            preview(temp.path(), &Selection::File("../outside".into()))
                .await
                .is_err()
        );
        std::os::unix::fs::symlink("/etc/passwd", temp.path().join("outside")).unwrap();
        let link = preview(temp.path(), &Selection::File("outside".into()))
            .await
            .unwrap();
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
