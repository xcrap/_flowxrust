use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Timestamp, now};

/// A workspace directory that owns threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "rootPath")]
    pub root_path: String,
    #[serde(default)]
    pub description: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Explicit thread ordering. Threads missing from this list are appended,
    /// so adding a thread never silently reshuffles the sidebar.
    #[serde(default, rename = "agentOrder")]
    pub agent_order: Vec<Uuid>,
}

impl Project {
    pub const DEFAULT_NAME: &'static str = "Untitled Project";

    pub fn new(root_path: impl Into<String>) -> Self {
        let root_path = root_path.into();
        let now = now();
        Self {
            id: Uuid::new_v4(),
            name: Self::default_name_for(&root_path),
            root_path,
            description: String::new(),
            created_at: now,
            updated_at: now,
            agent_order: Vec::new(),
        }
    }

    /// Derive a display name from the workspace path.
    pub fn default_name_for(root_path: &str) -> String {
        let normalized = normalize_root_path(root_path);
        std::path::Path::new(&normalized)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or(Self::DEFAULT_NAME)
            .to_string()
    }

    /// A stable spelling of the workspace path, safe to persist and to compare.
    ///
    /// Collapses `..`, `.`, and `~` so that two spellings of the same directory
    /// do not become two projects.
    pub fn normalized_root_path(&self) -> String {
        normalize_root_path(&self.root_path)
    }

    /// Resolve filesystem aliases when the target exists.
    ///
    /// Used for duplicate detection only. The normalized path stays the
    /// user-facing persisted spelling, because a symlink is how the user chose
    /// to name the workspace.
    pub fn canonical_root_path(&self) -> std::path::PathBuf {
        let normalized = self.normalized_root_path();
        match std::fs::canonicalize(&normalized) {
            Ok(p) => p,
            // The directory may not exist yet; fall back to the spelling we have.
            Err(_) => std::path::PathBuf::from(normalized),
        }
    }
}

/// Expand `~`, collapse `.` and `..`, and make the result absolute.
pub fn normalize_root_path(path: &str) -> String {
    let trimmed = path.trim();
    let candidate = if trimmed.is_empty() {
        home_dir()
    } else {
        trimmed.to_string()
    };
    let expanded = expand_tilde(&candidate);
    let absolute = if std::path::Path::new(&expanded).is_absolute() {
        std::path::PathBuf::from(&expanded)
    } else {
        std::path::PathBuf::from(home_dir()).join(expanded)
    };

    let mut out = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out.to_string_lossy().into_owned()
}

/// Expand a leading `~` to the home directory.
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{}/{}", home_dir(), rest);
    }
    path.to_string()
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

/// Accept a persisted path only if it is plausibly a real workspace.
///
/// Returns the normalized spelling, or `None` for anything that should not
/// become a project: blank, relative, or containing a `~` that did not expand.
pub fn validated_persisted_root_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || !std::path::Path::new(trimmed).is_absolute() {
        return None;
    }
    let normalized = normalize_root_path(trimmed);
    // Reject a path that is only the filesystem root: almost always a sign the
    // value was corrupted rather than that the user meant their whole disk.
    if normalized == "/" {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_expands_a_leading_tilde() {
        let home = home_dir();
        let out = normalize_root_path("~/code/app");
        assert_eq!(out, format!("{home}/code/app"));
    }

    #[test]
    fn normalization_collapses_dot_segments() {
        assert_eq!(normalize_root_path("/a/b/../c/./d"), "/a/c/d");
    }

    #[test]
    fn normalization_resolves_climbing_above_the_root_safely() {
        // `..` at the root must not escape to a nonsense path.
        assert_eq!(normalize_root_path("/../.."), "/");
    }

    #[test]
    fn a_relative_path_is_made_absolute_against_the_home_directory() {
        let home = home_dir();
        assert_eq!(normalize_root_path("code/x"), format!("{home}/code/x"));
    }

    #[test]
    fn an_empty_path_falls_back_to_the_home_directory() {
        assert_eq!(normalize_root_path("   "), home_dir());
    }

    #[test]
    fn the_default_name_is_the_last_path_component() {
        assert_eq!(Project::default_name_for("/Users/x/code/app"), "app");
        assert_eq!(Project::default_name_for("~/projects/flowx/"), "flowx");
    }

    #[test]
    fn validation_rejects_paths_that_should_not_become_projects() {
        assert!(validated_persisted_root_path("").is_none());
        assert!(validated_persisted_root_path("   ").is_none());
        assert!(validated_persisted_root_path("relative/path").is_none());
        assert!(
            validated_persisted_root_path("~/code").is_none(),
            "an unexpanded ~ is not a real path"
        );
        assert!(validated_persisted_root_path("/").is_none());
    }

    #[test]
    fn validation_accepts_and_normalizes_a_real_path() {
        assert_eq!(
            validated_persisted_root_path("/Users/x/code/../code/app").as_deref(),
            Some("/Users/x/code/app")
        );
    }

    #[test]
    fn two_spellings_of_one_directory_normalize_alike() {
        assert_eq!(
            normalize_root_path("/a/b/c"),
            normalize_root_path("/a/b/./c/../c")
        );
    }

    #[test]
    fn a_project_defaults_its_name_from_its_path() {
        let p = Project::new("/Users/x/code/thing");
        assert_eq!(p.name, "thing");
        assert!(p.agent_order.is_empty());
        assert_eq!(p.created_at, p.updated_at);
    }
}
