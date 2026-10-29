//! Dedicated chat checkouts. Creation never stages, stashes, resets or cleans
//! the source directory. Failed operations keep any files for recovery.

use super::{CommandSpec, run_lenient};
use crate::models::agent::ChatWorkspace;
use std::path::Path;
use std::time::Duration;

async fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = ["--no-pager", "-c", "core.hooksPath=/dev/null"]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect();
    let writing = args.iter().any(|arg| arg == "add");
    let output = run_lenient(
        CommandSpec::new(&args)
            .in_directory(Some(root))
            .with_timeout(Duration::from_secs(if writing { 120 } else { 10 }))
            .with_max_output_bytes(256 * 1024),
    )
    .await
    .map_err(|e| e.to_string())?;
    if !output.success() || output.truncated {
        return Err(format!("{}{}", output.stdout, output.stderr)
            .trim()
            .to_string());
    }
    Ok(output.stdout.trim().to_string())
}

pub async fn create(
    root: &Path,
    destination: &Path,
    branch: &str,
) -> Result<ChatWorkspace, String> {
    if branch.is_empty() || branch.starts_with('-') || branch != branch.trim() {
        return Err("Enter a valid new branch name.".into());
    }
    git(root, &["check-ref-format", &format!("refs/heads/{branch}")])
        .await
        .map_err(|_| "Enter a valid new branch name.".to_string())?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("Project folder is unavailable: {e}"))?;
    let top = git(&root, &["rev-parse", "--show-toplevel"]).await?;
    if Path::new(&top).canonicalize().ok().as_ref() != Some(&root) {
        return Err("Open the repository root as a project before creating a worktree.".into());
    }
    if !destination.is_absolute() || destination.symlink_metadata().is_ok() {
        return Err("The new worktree needs an unused absolute folder.".into());
    }
    let base_commit = git(&root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(|_| "Make an initial commit before creating a worktree.".to_string())?;
    let common_dir = common_dir(&root).await?;
    let path = destination
        .to_str()
        .ok_or("The worktree path is not valid Unicode.")?;
    git(&root, &["worktree", "add", "--no-track", "-b", branch, "--", path, &base_commit])
        .await.map_err(|error| format!("Could not create the worktree: {error}\nCheck {path} and branch {branch} before retrying. Existing files are kept."))?;
    let workspace = ChatWorkspace {
        path: destination
            .canonicalize()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned(),
        repository: root.to_string_lossy().into_owned(),
        common_dir,
        branch: branch.into(),
        base_commit,
    };
    validate(&workspace).await?;
    Ok(workspace)
}

async fn common_dir(root: &Path) -> Result<String, String> {
    let path = git(root, &["rev-parse", "--git-common-dir"]).await?;
    Ok(root
        .join(path)
        .canonicalize()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned())
}

/// Fail closed when a checkout was moved, removed, or replaced by another repo.
pub async fn validate(workspace: &ChatWorkspace) -> Result<(), String> {
    let root = Path::new(&workspace.path);
    if !root.is_absolute() || !root.join(".git").is_file() {
        return Err(format!(
            "The chat's worktree is unavailable: {}. Restore it before continuing.",
            workspace.path
        ));
    }
    if common_dir(root).await? != workspace.common_dir {
        return Err("This folder no longer belongs to the chat's repository. Restore its worktree before continuing.".into());
    }
    let canonical = root.canonicalize().map_err(|e| e.to_string())?;
    if canonical != root {
        return Err("The chat\'s saved worktree path now points to another folder. Restore it before continuing.".into());
    }
    let registered = git(root, &["worktree", "list", "--porcelain", "-z"]).await?;
    if !registered
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .any(|path| Path::new(path).canonicalize().ok().as_ref() == Some(&canonical))
    {
        return Err("This folder is no longer a registered worktree for the chat.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repository(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]).await.unwrap();
        git(path, &["config", "user.name", "FlowX test"])
            .await
            .unwrap();
        git(path, &["config", "user.email", "qa@example.invalid"])
            .await
            .unwrap();
        git(path, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        std::fs::write(path.join("sample"), "committed").unwrap();
        git(path, &["add", "sample"]).await.unwrap();
        git(path, &["commit", "-qm", "Initial"]).await.unwrap();
    }

    #[tokio::test]
    async fn worktrees_start_from_head_and_leave_dirty_files_and_index_intact() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        repository(&root).await;
        std::fs::write(root.join("sample"), "staged").unwrap();
        git(&root, &["add", "sample"]).await.unwrap();
        std::fs::write(root.join("sample"), "unstaged").unwrap();
        std::fs::write(root.join("untracked"), "keep").unwrap();
        let before = git(&root, &["status", "--porcelain=v1"]).await.unwrap();
        let work = create(&root, &temp.path().join("checkout"), "flowx/test")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(Path::new(&work.path).join("sample")).unwrap(),
            "committed"
        );
        assert!(!Path::new(&work.path).join("untracked").exists());
        assert_eq!(
            git(&root, &["status", "--porcelain=v1"]).await.unwrap(),
            before
        );
        assert_eq!(git(&root, &["show", ":sample"]).await.unwrap(), "staged");
        assert_eq!(
            git(&root, &["branch", "--show-current"]).await.unwrap(),
            "main"
        );
        validate(&work).await.unwrap();
        assert!(
            create(&root, &temp.path().join("another"), "flowx/test")
                .await
                .is_err()
        );
        assert!(
            create(&root, &temp.path().join("bad"), "--force")
                .await
                .is_err()
        );
        assert!(create(&root, &root, "flowx/overwrite").await.is_err());
        std::fs::rename(&work.path, temp.path().join("moved")).unwrap();
        assert!(validate(&work).await.is_err());
        repository(Path::new(&work.path)).await;
        assert!(validate(&work).await.is_err());
    }

    #[tokio::test]
    async fn unborn_repositories_and_non_root_projects_get_a_clear_error() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]).await.unwrap();
        let error = create(temp.path(), &temp.path().join("work"), "flowx/new")
            .await
            .unwrap_err();
        assert!(error.contains("initial commit"), "{error}");
        repository(temp.path()).await;
        std::fs::create_dir(temp.path().join("subdir")).unwrap();
        let error = create(
            &temp.path().join("subdir"),
            &temp.path().join("work"),
            "flowx/new",
        )
        .await
        .unwrap_err();
        assert!(error.contains("repository root"), "{error}");
    }
}
