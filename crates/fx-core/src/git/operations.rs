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
    /// Commit only the chosen paths and leave everything else, staged or not,
    /// where it is.
    Paths {
        /// Paths with work outside the index, which must be staged first.
        stage: Vec<String>,
        /// Every path the commit is limited to, including a rename's source,
        /// whose deletion would otherwise stay behind.
        commit: Vec<String>,
    },
}

/// The git invocations one commit needs, before the shared global options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPlan {
    pub stage: Option<Vec<String>>,
    pub commit: Vec<String>,
}

impl CommitPlan {
    pub fn new(message: &str, scope: &CommitScope) -> Result<Self, String> {
        if message.trim().is_empty() {
            return Err("Enter a commit message.".into());
        }
        // `-m` consumes the next argument whole, so a message beginning with a
        // dash is still a message.
        let mut commit = vec!["commit".to_string(), "-m".into(), message.to_string()];
        let stage = match scope {
            CommitScope::All => Some(vec!["add".into(), "-A".into()]),
            CommitScope::Staged => None,
            CommitScope::Paths {
                stage,
                commit: only,
            } => {
                if only.is_empty() {
                    return Err("Choose at least one file to commit.".into());
                }
                for path in stage.iter().chain(only) {
                    validate_relative_path(path)?;
                }
                // `--only` commits these paths' working-tree content and
                // nothing else from the index.
                commit.extend(["--only".into(), "--".into()]);
                commit.extend(only.iter().cloned());
                (!stage.is_empty()).then(|| {
                    let mut add = vec!["add".to_string(), "-A".into(), "--".into()];
                    add.extend(stage.iter().cloned());
                    add
                })
            }
        };
        Ok(Self { stage, commit })
    }
}

/// Stage per the plan, then commit. Returns git's summary line, such as
/// `[main 1a2b3c4] Fix the parser`.
pub async fn commit(root: &Path, plan: &CommitPlan) -> Result<String, String> {
    if let Some(stage) = &plan.stage {
        write(root, stage, "Staging failed.").await?;
    }
    let output = write(root, &plan.commit, "Commit failed.").await?;
    Ok(first_line(&output).unwrap_or_else(|| "Committed.".into()))
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
    let arguments = global_arguments(arguments);
    let mut spec = CommandSpec::new(&arguments)
        .in_directory(Some(root))
        .with_timeout(WRITE_TIMEOUT);
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
                stage: vec![],
                commit: vec![path.into()],
            };
            assert!(CommitPlan::new("message", &scope).is_err(), "{path:?}");
        }
        let plan = CommitPlan::new(
            "-n starts like an option",
            &CommitScope::Paths {
                stage: vec!["-f".into()],
                commit: vec!["-f".into()],
            },
        )
        .unwrap();
        assert_eq!(plan.stage.unwrap(), ["add", "-A", "--", "-f"]);
        assert_eq!(
            plan.commit,
            [
                "commit",
                "-m",
                "-n starts like an option",
                "--only",
                "--",
                "-f"
            ]
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
                stage: vec!["chosen.txt".into()],
                commit: vec!["renamed.txt".into(), "old.txt".into(), "chosen.txt".into()],
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
