//! The three operations that change a repository: stage, commit and push.
//!
//! Everything else FlowX does to a repository is a read, so these carry rules
//! of their own on top of the hygiene in [`super::command`]:
//!
//! - **Paths never reach git as options or globs.** Every path follows `--`,
//!   and every invocation runs under `--literal-pathspecs`, so a file named
//!   `-f` or `*` means exactly that file.
//! - **Never a force push.** No plan produced here contains a force flag or a
//!   `+` refspec; a rejected push is reported, not retried harder.
//! - **Failures are git's own words.** The whole combined output is returned,
//!   because a hook's explanation or a rejected ref's hint is what the user
//!   needs to act on, and a first line alone usually omits it.
//!
//! The user's own hooks, signing and credential helpers still run: these are
//! the user's commits, made the way their git makes them.

use std::path::{Component, Path};
use std::time::Duration;

use super::command::{self, CommandSpec};

/// A commit runs hooks and a push talks to a server; both get far longer than
/// a status read before FlowX gives up on them.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// The configuration reads that plan a push are local and quick.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What a commit includes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitScope {
    /// Stage every change, new files included, then commit everything.
    All,
    /// Commit the index exactly as the user built it, for example with
    /// `git add -p`.
    Staged,
    /// Commit only the chosen paths, as they are in the working tree, and
    /// leave everything else, staged or not, where it is.
    Paths {
        /// The chosen paths.
        chosen: Vec<String>,
        /// The sources of chosen renames, committed as removed. Whatever sits
        /// at that path now is a different file, and it was not chosen.
        sources: Vec<String>,
    },
}

/// The git invocations one commit needs, before the shared global options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    /// Staging into the user's own index, for a commit of everything.
    pub stage: Option<Vec<String>>,
    pub commit: Vec<String>,
    /// A chosen-files commit, which is built in an index of FlowX's own.
    pub chosen: Option<ChosenFiles>,
}

/// What a chosen-files commit takes, and from where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenFiles {
    /// Committed as they are in the working tree.
    pub paths: Vec<String>,
    /// Committed as removed.
    pub sources: Vec<String>,
}

impl CommitPlan {
    pub fn new(message: &str, scope: &CommitScope) -> Result<Self, String> {
        if message.trim().is_empty() {
            return Err("Enter a commit message.".into());
        }
        // `-m` consumes the next argument whole, so a message beginning with a
        // dash is still a message.
        let commit = vec!["commit".to_string(), "-m".into(), message.to_string()];
        let (stage, chosen) = match scope {
            CommitScope::All => (Some(vec!["add".into(), "-A".into()]), None),
            CommitScope::Staged => (None, None),
            CommitScope::Paths { chosen, sources } => {
                if chosen.is_empty() {
                    return Err("Choose at least one file to commit.".into());
                }
                for path in chosen.iter().chain(sources) {
                    validate_relative_path(path)?;
                }
                // A source that was chosen as well is a file of its own now,
                // and is committed as one.
                let sources = sources
                    .iter()
                    .filter(|source| !chosen.contains(source))
                    .cloned()
                    .collect();
                let chosen = ChosenFiles {
                    paths: chosen.clone(),
                    sources,
                };
                (None, Some(chosen))
            }
        };
        Ok(Self {
            stage,
            commit,
            chosen,
        })
    }
}

/// Stage per the plan, then commit. Returns git's summary line, such as
/// `[main 1a2b3c4] Fix the parser`.
pub async fn commit(root: &Path, plan: &CommitPlan) -> Result<String, String> {
    if let Some(chosen) = &plan.chosen {
        return commit_chosen(root, &plan.commit, chosen).await;
    }
    if let Some(stage) = &plan.stage {
        write(root, stage, "Staging failed.").await?;
    }
    let output = write(root, &plan.commit, "Commit failed.").await?;
    Ok(first_line(&output).unwrap_or_else(|| "Committed.".into()))
}

/// Commit chosen files through an index of FlowX's own.
///
/// `git commit --only` takes every path it is given from the working tree,
/// and a rename's source has to be given or its deletion stays behind. After
/// `git mv old new` and a new, untracked `old`, that committed the untracked
/// file as `old`'s new content. A temporary index lets each path come from
/// where it should: a chosen file from the working tree, a rename's source as
/// removed. The user's own index is then brought up to the commit for those
/// paths and no others, which is what `--only` leaves behind as well, and a
/// failed commit leaves it exactly as it was.
async fn commit_chosen(
    root: &Path,
    commit: &[String],
    chosen: &ChosenFiles,
) -> Result<String, String> {
    if let Some(refusal) = partial_commit_refusal(root).await {
        return Err(refusal);
    }
    const PREPARE: &str = "Preparing the commit failed.";
    let index = TemporaryIndex::beside(root).await.ok_or(PREPARE)?;
    let index_path = Some(index.0.as_path());
    let born = read(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .is_some();
    let base: Vec<String> = if born {
        vec!["read-tree".into(), "HEAD".into()]
    } else {
        vec!["read-tree".into(), "--empty".into()]
    };
    write_in(root, index_path, &base, PREPARE).await?;

    // A chosen path that is gone from the working tree is a deletion, and
    // `add` refuses a path that matches nothing at all, as an added file
    // deleted again does.
    let (present, missing): (Vec<String>, Vec<String>) = chosen
        .paths
        .iter()
        .cloned()
        .partition(|path| root.join(path).symlink_metadata().is_ok());
    if !present.is_empty() {
        let mut add = vec!["add".to_string(), "-A".into(), "--".into()];
        add.extend(present);
        write_in(root, index_path, &add, "Staging failed.").await?;
    }
    let removed: Vec<String> = missing
        .into_iter()
        .chain(chosen.sources.iter().cloned())
        .collect();
    if !removed.is_empty() {
        let mut remove = vec![
            "rm".to_string(),
            "--cached".into(),
            "--quiet".into(),
            "--ignore-unmatch".into(),
            "--".into(),
        ];
        remove.extend(removed);
        write_in(root, index_path, &remove, "Staging failed.").await?;
    }

    let output = write_in(root, index_path, commit, "Commit failed.").await?;
    let summary = first_line(&output).unwrap_or_else(|| "Committed.".into());
    let mut reset = vec!["reset".to_string(), "--quiet".into(), "--".into()];
    reset.extend(chosen.paths.iter().chain(&chosen.sources).cloned());
    if let Err(error) = write(root, &reset, "Updating the index failed.").await {
        return Err(format!(
            "{summary}\n\nThe commit was made, but the index could not be brought up to it:\n{error}"
        ));
    }
    Ok(summary)
}

/// Why a chosen-files commit cannot be made now, in git's words.
///
/// The commit in [`commit_chosen`] names no pathspec, so git's own refusal of
/// a partial commit mid-merge never runs. Made anyway, it was a two-parent
/// merge commit holding `HEAD` plus the chosen files: `MERGE_HEAD` was
/// consumed, and the other branch counted as merged with its changes dropped.
/// A cherry-pick lent the commit the picked commit's author, and a revert
/// ended with only the chosen files reverted. Git itself allows the last, and
/// a partial commit beside unmerged files, which commits conflict markers if
/// one of them is chosen; both are refused here, as a plain `git commit`
/// refuses them.
async fn partial_commit_refusal(root: &Path) -> Option<String> {
    for (head, operation) in [
        ("MERGE_HEAD", "a merge"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
    ] {
        if read(root, &["rev-parse", "--quiet", "--verify", head])
            .await
            .is_some()
        {
            return Some(format!("cannot do a partial commit during {operation}."));
        }
    }
    read(root, &["ls-files", "--unmerged"]).await.map(|_| {
        "Committing is not possible because you have unmerged files.\n\
         Fix them up in the work tree, and then use 'git add/rm <file>' as \
         appropriate to mark resolution and make a commit."
            .to_string()
    })
}

/// An index file beside the repository's own, removed when dropped.
///
/// Beside it rather than in the system's temporary directory, so it has the
/// repository's permissions: an index lists every path in the tree.
struct TemporaryIndex(std::path::PathBuf);

impl TemporaryIndex {
    async fn beside(root: &Path) -> Option<Self> {
        let own = read(root, &["rev-parse", "--git-path", "index"]).await?;
        let path = root.join(format!("{own}.flowx-{}", uuid::Uuid::new_v4().simple()));
        Some(Self(path))
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Everything that decides how to push the current branch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushContext {
    pub branch: Option<String>,
    /// `branch.<name>.merge`. Unlike `@{upstream}` it still resolves once the
    /// remote branch is gone, which is the state of a fresh empty clone.
    pub merge_ref: Option<String>,
    pub branch_push_remote: Option<String>,
    pub push_default: Option<String>,
    pub remotes: Vec<String>,
}

/// Decide the push invocation.
///
/// A bare `git push` fails with "has no upstream branch" unless
/// `push.autoSetupRemote` is set, so a branch without one is published
/// explicitly and starts tracking what was pushed. The remote is the branch's
/// push remote, then `remote.pushDefault`, then the only remote, then `origin`.
pub fn push_arguments(context: &PushContext) -> Result<Vec<String>, String> {
    if !context.branch.as_deref().is_some_and(|b| !b.is_empty()) {
        return Err("Check out a branch before pushing.".into());
    }
    if context.merge_ref.as_deref().is_some_and(|m| !m.is_empty()) {
        return Ok(vec!["push".into()]);
    }
    let remote = context
        .branch_push_remote
        .clone()
        .or_else(|| context.push_default.clone())
        .filter(|remote| !remote.is_empty())
        .or_else(|| (context.remotes.len() == 1).then(|| context.remotes[0].clone()))
        .unwrap_or_else(|| "origin".into());
    // `--` keeps a remote named like an option from being read as one.
    Ok(vec![
        "push".into(),
        "--set-upstream".into(),
        "--".into(),
        remote,
        "HEAD".into(),
    ])
}

/// Push the current branch. Returns git's account of what moved.
pub async fn push(root: &Path) -> Result<String, String> {
    let context = push_context(root).await;
    let arguments = push_arguments(&context)?;
    let output = write(root, &arguments, "Push failed.").await?;
    Ok(push_summary(&output))
}

async fn push_context(root: &Path) -> PushContext {
    let branch = read(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
    let config = |key: String| async move { read(root, &["config", "--get", &key]).await };
    let (merge_ref, branch_push_remote) = match &branch {
        Some(branch) => (
            config(format!("branch.{branch}.merge")).await,
            config(format!("branch.{branch}.pushRemote")).await,
        ),
        None => (None, None),
    };
    let push_default = config("remote.pushDefault".into()).await;
    let remotes = read(root, &["remote"])
        .await
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    PushContext {
        branch,
        merge_ref,
        branch_push_remote,
        push_default,
        remotes,
    }
}

/// The line naming the moved ref, or git's own "Everything up-to-date".
pub fn push_summary(output: &str) -> String {
    let moved = output
        .lines()
        .find(|line| line.contains(" -> "))
        .map(|line| {
            let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("Pushed {}", collapsed.trim_start_matches(['*', '+', ' ']))
        });
    moved
        .or_else(|| first_line(output).filter(|line| line.contains("up-to-date")))
        .unwrap_or_else(|| "Pushed.".into())
}

/// Options every write shares: no pager, literal pathspecs, and no
/// filesystem monitor, which is a program the configuration can name.
fn global_arguments(arguments: &[impl AsRef<str>]) -> Vec<String> {
    [
        "--no-pager",
        "--literal-pathspecs",
        "-c",
        "core.fsmonitor=false",
    ]
    .into_iter()
    .map(String::from)
    .chain(arguments.iter().map(|a| a.as_ref().to_string()))
    .collect()
}

async fn write(root: &Path, arguments: &[String], fallback: &str) -> Result<String, String> {
    write_in(root, None, arguments, fallback).await
}

/// [`write`], against `index` in place of the repository's own when given.
async fn write_in(
    root: &Path,
    index: Option<&Path>,
    arguments: &[String],
    fallback: &str,
) -> Result<String, String> {
    let arguments = global_arguments(arguments);
    let mut spec = CommandSpec::new(&arguments)
        .in_directory(Some(root))
        .with_timeout(WRITE_TIMEOUT)
        .with_index_file(index);
    spec.include_stderr_in_stdout = true;
    match command::run_lenient(spec).await {
        Ok(output) if output.success() => Ok(output.stdout),
        Ok(output) => {
            let text = output.stdout.trim();
            Err(if text.is_empty() {
                format!("{fallback} git exited with status {}.", output.status)
            } else {
                text.to_string()
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

/// A configuration read: its trimmed output, or `None` when unset.
async fn read(root: &Path, arguments: &[&str]) -> Option<String> {
    let arguments = global_arguments(arguments);
    let spec = CommandSpec::new(&arguments)
        .in_directory(Some(root))
        .with_timeout(READ_TIMEOUT);
    let output = command::run_lenient(spec).await.ok()?;
    let text = output.stdout.trim();
    (output.success() && !text.is_empty()).then(|| text.to_string())
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(format!("{path:?} is outside the project."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GIT;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new(GIT)
            .args(args)
            .current_dir(root)
            .envs(command::hardened_environment())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn identify(root: &Path) {
        git(
            root,
            &["config", "user.email", "flowx-test@example.invalid"],
        );
        git(root, &["config", "user.name", "FlowX test"]);
        git(root, &["config", "commit.gpgsign", "false"]);
    }

    fn repository() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        identify(temp.path());
        std::fs::write(temp.path().join("tracked.txt"), "one\n").unwrap();
        std::fs::write(temp.path().join("old.txt"), "moving\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        temp
    }

    fn status(root: &Path) -> String {
        git(root, &["status", "--porcelain=v1"])
    }

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn no_push_plan_ever_forces() {
        let contexts = [
            PushContext {
                branch: Some("main".into()),
                merge_ref: Some("refs/heads/main".into()),
                ..Default::default()
            },
            PushContext {
                branch: Some("main".into()),
                remotes: vec!["upstream".into()],
                ..Default::default()
            },
            PushContext {
                branch: Some("main".into()),
                branch_push_remote: Some("--force".into()),
                ..Default::default()
            },
        ];
        for context in contexts {
            let arguments = push_arguments(&context).unwrap();
            assert_eq!(arguments[0], "push");
            // After `--` git reads operands, so only what precedes it can be
            // an option; a remote named `--force` is just a (missing) remote.
            let separator = arguments
                .iter()
                .position(|a| a == "--")
                .unwrap_or(arguments.len());
            for argument in &arguments[..separator] {
                assert!(
                    !argument.starts_with("-f") && !argument.starts_with("--force"),
                    "{arguments:?}"
                );
            }
            for argument in &arguments[separator..] {
                assert!(
                    !argument.starts_with('+'),
                    "a forced refspec: {arguments:?}"
                );
            }
        }
    }

    #[test]
    fn a_branch_without_an_upstream_is_published_to_the_resolved_remote() {
        let base = PushContext {
            branch: Some("feature".into()),
            remotes: vec!["origin".into(), "fork".into()],
            ..Default::default()
        };
        let remote = |context: &PushContext| push_arguments(context).unwrap()[3].clone();
        assert_eq!(
            remote(&base),
            "origin",
            "several remotes fall back to origin"
        );
        let only = PushContext {
            remotes: vec!["upstream".into()],
            ..base.clone()
        };
        assert_eq!(remote(&only), "upstream");
        let default = PushContext {
            push_default: Some("fork".into()),
            ..base.clone()
        };
        assert_eq!(remote(&default), "fork");
        let branch = PushContext {
            branch_push_remote: Some("mine".into()),
            push_default: Some("fork".into()),
            ..base.clone()
        };
        assert_eq!(remote(&branch), "mine");
        assert_eq!(
            push_arguments(&PushContext::default()).unwrap_err(),
            "Check out a branch before pushing."
        );
    }

    #[test]
    fn commit_plans_reject_empty_messages_and_escaping_paths() {
        assert!(CommitPlan::new("  \n", &CommitScope::All).is_err());
        for path in ["../outside", "/etc/passwd", ""] {
            let scope = CommitScope::Paths {
                chosen: vec![path.into()],
                sources: vec![],
            };
            assert!(CommitPlan::new("message", &scope).is_err(), "{path:?}");
            let scope = CommitScope::Paths {
                chosen: vec!["inside".into()],
                sources: vec![path.into()],
            };
            assert!(CommitPlan::new("message", &scope).is_err(), "{path:?}");
        }
        let plan = CommitPlan::new(
            "-n starts like an option",
            &CommitScope::Paths {
                chosen: vec!["-f".into(), "new".into()],
                sources: vec!["old".into(), "new".into()],
            },
        )
        .unwrap();
        assert_eq!(plan.stage, None, "the user's index is not staged into");
        assert_eq!(plan.commit, ["commit", "-m", "-n starts like an option"]);
        assert_eq!(
            plan.chosen.unwrap(),
            ChosenFiles {
                paths: vec!["-f".into(), "new".into()],
                sources: vec!["old".into()],
            },
            "a source that was chosen too is committed as a file"
        );
    }

    #[test]
    fn committing_all_stages_new_and_modified_files() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let plan = CommitPlan::new("Everything", &CommitScope::All).unwrap();
        let summary = run(commit(root, &plan)).unwrap();
        assert!(summary.contains("Everything"), "{summary}");
        assert_eq!(status(root), "");
    }

    #[test]
    fn committing_staged_leaves_unstaged_work_alone() {
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "staged and more\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let plan = CommitPlan::new("Only the index", &CommitScope::Staged).unwrap();
        run(commit(root, &plan)).unwrap();
        assert_eq!(git(root, &["show", "HEAD:tracked.txt"]), "staged\n");
        assert_eq!(status(root), " M tracked.txt\n?? new.txt\n");
    }

    #[test]
    fn committing_selected_paths_includes_a_rename_source_and_leaves_the_rest() {
        let temp = repository();
        let root = temp.path();
        git(root, &["mv", "old.txt", "renamed.txt"]);
        std::fs::write(root.join("tracked.txt"), "unselected but staged\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("chosen.txt"), "new and chosen\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "new, not chosen\n").unwrap();
        let plan = CommitPlan::new(
            "Selected",
            &CommitScope::Paths {
                chosen: vec!["renamed.txt".into(), "chosen.txt".into()],
                sources: vec!["old.txt".into()],
            },
        )
        .unwrap();
        run(commit(root, &plan)).unwrap();
        let committed = git(root, &["show", "--name-status", "--format=", "HEAD"]);
        assert!(committed.contains("chosen.txt"), "{committed}");
        assert!(committed.contains("renamed.txt"), "{committed}");
        assert!(
            committed.contains("old.txt"),
            "the rename's source must be committed too: {committed}"
        );
        assert!(!committed.contains("tracked.txt"), "{committed}");
        assert_eq!(status(root), "M  tracked.txt\n?? ignored.txt\n");
        assert_no_temporary_index(root);
    }

    #[test]
    fn an_untracked_file_at_a_rename_source_is_not_committed() {
        // `git commit --only -- new.txt old.txt` took `old.txt` from the
        // working tree, which after `git mv` and a fresh `old.txt` committed
        // the new, unchosen file as the old one's content.
        let temp = repository();
        let root = temp.path();
        git(root, &["mv", "old.txt", "new.txt"]);
        std::fs::write(root.join("old.txt"), "an unrelated new file\n").unwrap();
        std::fs::write(root.join("tracked.txt"), "staged, not chosen\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        let plan = CommitPlan::new(
            "Rename",
            &CommitScope::Paths {
                chosen: vec!["new.txt".into()],
                sources: vec!["old.txt".into()],
            },
        )
        .unwrap();
        run(commit(root, &plan)).unwrap();
        let committed = git(root, &["show", "--name-status", "--format=", "HEAD"]);
        assert!(
            committed
                .lines()
                .any(|line| line.starts_with('R') && line.ends_with("old.txt\tnew.txt")),
            "the commit is the rename: {committed}"
        );
        assert_eq!(git(root, &["show", "HEAD:new.txt"]), "moving\n");
        assert!(
            git(root, &["ls-tree", "--name-only", "HEAD"])
                .lines()
                .all(|name| name != "old.txt"),
            "the untracked file was committed"
        );
        assert_eq!(status(root), "M  tracked.txt\n?? old.txt\n");
        assert_no_temporary_index(root);
    }

    #[test]
    fn a_chosen_deletion_and_a_partly_staged_file_commit_their_working_tree() {
        let temp = repository();
        let root = temp.path();
        std::fs::remove_file(root.join("old.txt")).unwrap();
        std::fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "staged and more\n").unwrap();
        // Added, then deleted again: nothing to commit, and nothing to fail on.
        std::fs::write(root.join("brief.txt"), "brief\n").unwrap();
        git(root, &["add", "brief.txt"]);
        std::fs::remove_file(root.join("brief.txt")).unwrap();
        let plan = CommitPlan::new(
            "Both",
            &CommitScope::Paths {
                chosen: vec!["old.txt".into(), "tracked.txt".into(), "brief.txt".into()],
                sources: vec![],
            },
        )
        .unwrap();
        run(commit(root, &plan)).unwrap();
        assert_eq!(
            git(root, &["show", "HEAD:tracked.txt"]),
            "staged and more\n"
        );
        assert_eq!(
            git(root, &["show", "--name-status", "--format=", "HEAD"]),
            "D\told.txt\nM\ttracked.txt\n"
        );
        assert_eq!(status(root), "");
    }

    #[test]
    fn a_rejected_chosen_commit_leaves_the_index_alone() {
        let temp = repository();
        let root = temp.path();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho 'the hook says no' >&2\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.join("tracked.txt"), "edited\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let plan = CommitPlan::new(
            "Refused",
            &CommitScope::Paths {
                chosen: vec!["tracked.txt".into(), "new.txt".into()],
                sources: vec![],
            },
        )
        .unwrap();
        let error = run(commit(root, &plan)).unwrap_err();
        assert!(error.contains("the hook says no"), "{error}");
        assert_eq!(status(root), " M tracked.txt\n?? new.txt\n");
        assert_no_temporary_index(root);
    }

    #[test]
    fn a_chosen_commit_works_on_an_unborn_branch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q", "-b", "main"]);
        identify(root);
        std::fs::write(root.join("first.txt"), "first\n").unwrap();
        std::fs::write(root.join("later.txt"), "later\n").unwrap();
        let plan = CommitPlan::new(
            "First",
            &CommitScope::Paths {
                chosen: vec!["first.txt".into()],
                sources: vec![],
            },
        )
        .unwrap();
        run(commit(root, &plan)).unwrap();
        assert_eq!(
            git(root, &["ls-tree", "--name-only", "HEAD"]),
            "first.txt\n"
        );
        assert_eq!(status(root), "?? later.txt\n");
    }

    /// Run git where failing is the point, as a conflicted merge does.
    fn git_may_fail(root: &Path, args: &[&str]) {
        std::process::Command::new(GIT)
            .args(args)
            .current_dir(root)
            .envs(command::hardened_environment())
            .output()
            .unwrap();
    }

    /// `tracked.txt` changed one way on `side` and another on `main`, so
    /// bringing `side` in conflicts. Returns `side`'s commit.
    fn diverge(root: &Path) -> String {
        git(root, &["checkout", "-q", "-b", "side"]);
        std::fs::write(root.join("tracked.txt"), "theirs\n").unwrap();
        git(root, &["commit", "-qam", "theirs"]);
        git(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("tracked.txt"), "ours\n").unwrap();
        git(root, &["commit", "-qam", "ours"]);
        git(root, &["rev-parse", "side"]).trim().to_string()
    }

    fn chosen_old_txt() -> CommitPlan {
        CommitPlan::new(
            "Only this",
            &CommitScope::Paths {
                chosen: vec!["old.txt".into()],
                sources: vec![],
            },
        )
        .unwrap()
    }

    fn resolves(root: &Path, name: &str) -> bool {
        std::process::Command::new(GIT)
            .args(["rev-parse", "--quiet", "--verify", name])
            .current_dir(root)
            .envs(command::hardened_environment())
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn a_chosen_commit_is_refused_mid_merge_and_the_merge_survives() {
        // Made anyway, it was a merge commit of HEAD plus `old.txt`: the
        // conflict and `side`'s changes were dropped, and `side` counted as
        // merged.
        let temp = repository();
        let root = temp.path();
        let side = diverge(root);
        git_may_fail(root, &["merge", "--no-edit", "side"]);
        std::fs::write(root.join("old.txt"), "an unrelated edit\n").unwrap();
        let head = git(root, &["rev-parse", "HEAD"]);
        let before = status(root);
        assert!(before.contains("UU tracked.txt"), "{before}");

        let error = run(commit(root, &chosen_old_txt())).unwrap_err();
        assert_eq!(error, "cannot do a partial commit during a merge.");
        assert_eq!(git(root, &["rev-parse", "MERGE_HEAD"]).trim(), side);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(status(root), before, "the index is untouched");
        assert_no_temporary_index(root);

        // Resolved, the merge is still in progress and still refused.
        git(root, &["add", "tracked.txt"]);
        let error = run(commit(root, &chosen_old_txt())).unwrap_err();
        assert_eq!(error, "cannot do a partial commit during a merge.");
        assert!(resolves(root, "MERGE_HEAD"));
    }

    #[test]
    fn a_chosen_commit_is_refused_during_a_cherry_pick_or_a_revert() {
        let temp = repository();
        let root = temp.path();
        diverge(root);
        git_may_fail(root, &["cherry-pick", "side"]);
        // Resolved, so only the cherry-pick itself stands in the way.
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("old.txt"), "an unrelated edit\n").unwrap();
        let error = run(commit(root, &chosen_old_txt())).unwrap_err();
        assert_eq!(error, "cannot do a partial commit during a cherry-pick.");
        assert!(resolves(root, "CHERRY_PICK_HEAD"));
        git(root, &["cherry-pick", "--abort"]);

        // `ours` reverted onto a later edit of the same line conflicts.
        std::fs::write(root.join("tracked.txt"), "later\n").unwrap();
        git(root, &["commit", "-qam", "later"]);
        git_may_fail(root, &["revert", "--no-edit", "HEAD~1"]);
        git(root, &["add", "tracked.txt"]);
        std::fs::write(root.join("old.txt"), "an unrelated edit\n").unwrap();
        let error = run(commit(root, &chosen_old_txt())).unwrap_err();
        assert_eq!(error, "cannot do a partial commit during a revert.");
        assert!(
            resolves(root, "REVERT_HEAD"),
            "a commit would have ended the revert"
        );
        assert_no_temporary_index(root);
    }

    #[test]
    fn a_chosen_commit_is_refused_beside_unmerged_files() {
        // A conflicted `stash pop` leaves unmerged entries and no merge.
        let temp = repository();
        let root = temp.path();
        std::fs::write(root.join("tracked.txt"), "stashed\n").unwrap();
        git(root, &["stash", "-q"]);
        std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(root, &["commit", "-qam", "committed"]);
        git_may_fail(root, &["stash", "pop"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let plan = CommitPlan::new(
            "Markers",
            &CommitScope::Paths {
                chosen: vec!["tracked.txt".into()],
                sources: vec![],
            },
        )
        .unwrap();
        let error = run(commit(root, &plan)).unwrap_err();
        assert!(
            error.starts_with("Committing is not possible because you have unmerged files."),
            "{error}"
        );
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(status(root), "UU tracked.txt\n");
    }

    fn assert_no_temporary_index(root: &Path) {
        let left: Vec<_> = std::fs::read_dir(root.join(".git"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("flowx"))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }

    #[test]
    fn a_failed_commit_reports_gits_own_words() {
        let temp = repository();
        let plan = CommitPlan::new("Nothing here", &CommitScope::Staged).unwrap();
        let error = run(commit(temp.path(), &plan)).unwrap_err();
        assert!(error.contains("nothing to commit"), "{error}");
        assert!(
            error.lines().count() > 1,
            "the whole explanation, not only its first line: {error}"
        );
    }

    #[test]
    fn pushing_publishes_an_empty_clone_and_never_overwrites_a_diverged_remote() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let clone = temp.path().join("clone");
        let other = temp.path().join("other");
        git(
            temp.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        git(temp.path(), &["clone", "-q", "remote.git", "clone"]);
        identify(&clone);
        std::fs::write(clone.join("a.txt"), "first\n").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-qm", "first"]);
        // The empty clone tracks a branch the remote does not have yet.
        assert!(
            git(&clone, &["status", "--branch", "--porcelain=v1"]).contains("[gone]"),
            "fixture should start with a gone upstream"
        );
        let summary = run(push(&clone)).unwrap();
        assert!(summary.starts_with("Pushed"), "{summary}");
        assert_eq!(
            git(&remote, &["rev-parse", "main"]),
            git(&clone, &["rev-parse", "HEAD"])
        );

        // Someone else pushes, then this clone commits on the old tip.
        git(temp.path(), &["clone", "-q", "remote.git", "other"]);
        identify(&other);
        std::fs::write(other.join("b.txt"), "theirs\n").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-qm", "theirs"]);
        git(&other, &["push", "-q"]);
        let theirs = git(&remote, &["rev-parse", "main"]);
        std::fs::write(clone.join("a.txt"), "mine\n").unwrap();
        git(&clone, &["commit", "-qam", "mine"]);
        let error = run(push(&clone)).unwrap_err();
        assert!(error.contains("rejected"), "{error}");
        assert_eq!(
            git(&remote, &["rev-parse", "main"]),
            theirs,
            "a rejected push must leave the remote untouched"
        );
    }

    #[test]
    fn a_branch_without_an_upstream_is_published_and_then_tracks_it() {
        let temp = repository();
        let root = temp.path();
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "-q", "--bare"]);
        git(
            root,
            &["remote", "add", "origin", &remote.path().to_string_lossy()],
        );
        let summary = run(push(root)).unwrap();
        assert!(summary.contains("main"), "{summary}");
        assert_eq!(
            git(root, &["config", "--get", "branch.main.merge"]).trim(),
            "refs/heads/main"
        );
        assert_eq!(
            git(remote.path(), &["rev-parse", "main"]),
            git(root, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn push_summaries_name_the_moved_ref() {
        assert_eq!(
            push_summary("To /tmp/remote.git\n * [new branch]      HEAD -> main\n"),
            "Pushed [new branch] HEAD -> main"
        );
        assert_eq!(
            push_summary("To /tmp/r\n   1a2b3c4..5d6e7f8  main -> main\n"),
            "Pushed 1a2b3c4..5d6e7f8 main -> main"
        );
        assert_eq!(
            push_summary("Everything up-to-date\n"),
            "Everything up-to-date"
        );
    }
}
