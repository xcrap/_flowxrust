//! Where FlowX keeps its state.

use std::path::PathBuf;

use thiserror::Error;

/// The application-support directory could not be determined.
#[derive(Debug, Error)]
pub enum PathsError {
    #[error("could not locate the application support directory")]
    NoSupportDirectory,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Resolved locations for FlowX's persistent state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    root: PathBuf,
}

impl AppPaths {
    /// Resolve the directory for a build flavour.
    ///
    /// Debug and release use separate directories so running a dev build cannot
    /// overwrite real user data — the same rule the Swift original follows.
    pub fn discover(debug: bool) -> Result<Self, PathsError> {
        let base = directories::ProjectDirs::from("com", "flowx", "FlowX")
            .ok_or(PathsError::NoSupportDirectory)?;
        let name = if debug { "FlowX-Dev" } else { "FlowX" };
        Ok(Self {
            root: base.data_dir().join(name),
        })
    }

    /// Use an explicit directory, for tests and for a portable install.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The project and layout snapshot.
    pub fn projects_file(&self) -> PathBuf {
        self.root.join("projects.json")
    }

    pub fn preferences_file(&self) -> PathBuf {
        self.root.join("preferences.json")
    }

    /// Per-thread transcripts.
    pub fn conversations_dir(&self) -> PathBuf {
        self.root.join("conversations")
    }

    /// One thread's transcript.
    pub fn conversation_file(&self, project_id: uuid::Uuid, agent_id: uuid::Uuid) -> PathBuf {
        self.conversations_dir()
            .join(project_id.to_string())
            .join(format!("{agent_id}.json"))
    }

    /// Image sidecars for one thread.
    pub fn assets_dir(&self, project_id: uuid::Uuid, agent_id: uuid::Uuid) -> PathBuf {
        self.conversations_dir()
            .join(project_id.to_string())
            .join("assets")
            .join(agent_id.to_string())
    }

    /// Create the directory tree, with restrictive permissions.
    ///
    /// Everything here can contain prompts and code, so the directory is owner
    /// only by default rather than inheriting a wider mode.
    pub fn ensure(&self) -> std::io::Result<()> {
        create_private_dir(&self.root)
    }
}

/// Create a directory that only the owner can enter.
pub fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_private_mode(path)
}

#[cfg(unix)]
fn set_private_mode(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn projects_and_conversations_live_under_one_root() {
        let paths = AppPaths::at("/tmp/flowx-test");
        assert_eq!(
            paths.projects_file(),
            std::path::Path::new("/tmp/flowx-test/projects.json")
        );
        assert!(paths.conversations_dir().starts_with(paths.root()));
    }

    #[test]
    fn a_conversation_path_names_both_identifiers() {
        let paths = AppPaths::at("/tmp/flowx-test");
        let (project, agent) = (Uuid::nil(), Uuid::from_u128(7));
        let file = paths.conversation_file(project, agent);
        assert!(file.to_string_lossy().contains(&agent.to_string()));
        assert!(file.to_string_lossy().contains(&project.to_string()));
    }

    #[test]
    fn assets_are_nested_under_the_project() {
        let paths = AppPaths::at("/tmp/flowx-test");
        let assets = paths.assets_dir(Uuid::nil(), Uuid::from_u128(3));
        assert!(assets.to_string_lossy().contains("assets"));
    }

    #[test]
    fn debug_and_release_use_different_directories() {
        // A dev build must never overwrite real user data.
        let debug = AppPaths::discover(true).expect("support directory");
        let release = AppPaths::discover(false).expect("support directory");
        assert_ne!(debug.root(), release.root());
        assert!(debug.root().to_string_lossy().contains("FlowX-Dev"));
    }

    #[test]
    fn creating_the_tree_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        paths.ensure().unwrap();
        paths.ensure().unwrap();
        assert!(paths.root().is_dir());
    }
}
