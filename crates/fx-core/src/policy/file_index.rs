//! Which files the Files panel shows.
//!
//! The panel walks a workspace that may contain a checkout, a build tree, and a
//! dependency directory. Listing all of it is slow and useless, so these rules
//! skip directories that are never worth showing.

use std::path::Path;

/// Directories skipped wherever they appear in a relative path.
pub const SKIPPED_DIRECTORY_NAMES: [&str; 8] = [
    ".git",
    ".build",
    ".next",
    ".swiftpm",
    "DerivedData",
    "build",
    "dist",
    "node_modules",
];

pub struct ProjectFileIndexPolicy;

impl ProjectFileIndexPolicy {
    /// Whether a directory entry should not be descended into.
    pub fn should_skip(relative_path: &Path, is_directory: bool) -> bool {
        // Only directories can hide anything worth skipping.
        if !is_directory {
            return false;
        }
        relative_path.components().any(|component| {
            let name = component.as_os_str().to_string_lossy();
            Self::is_skipped_name(&name)
        })
    }

    /// Whether a file lives somewhere that would have been skipped.
    ///
    /// Takes the file's own path rather than its directory so a file named
    /// `build` is not mistaken for being inside one.
    pub fn should_skip_file(relative_path: &Path) -> bool {
        relative_path
            .parent()
            .is_some_and(|dir| Self::should_skip(dir, true))
    }

    fn is_skipped_name(name: &str) -> bool {
        SKIPPED_DIRECTORY_NAMES.contains(&name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_artifacts_are_skipped_wherever_they_appear() {
        for dir in [
            "node_modules",
            ".git",
            "build",
            "dist",
            "DerivedData",
            ".next",
            ".build",
        ] {
            assert!(
                ProjectFileIndexPolicy::should_skip(&PathBuf::from(dir), true),
                "{dir} was not skipped"
            );
            assert!(
                ProjectFileIndexPolicy::should_skip(&PathBuf::from("src").join(dir), true),
                "src/{dir} was not skipped"
            );
            assert!(
                ProjectFileIndexPolicy::should_skip(
                    &PathBuf::from("a/b").join(dir).join("c"),
                    true
                ),
                "a/b/{dir}/c was not skipped"
            );
        }
    }

    #[test]
    fn ordinary_directories_are_kept() {
        for dir in ["src", "app", "crates", "FlowX", "packages"] {
            assert!(
                !ProjectFileIndexPolicy::should_skip(&PathBuf::from(dir), true),
                "{dir} was wrongly skipped"
            );
        }
    }

    #[test]
    fn a_directory_whose_name_similar_to_a_skipped_one_is_kept() {
        // Substring matching would drop these.
        for dir in ["buildings", "mybuild", "node_modules_old", "distillery"] {
            assert!(
                !ProjectFileIndexPolicy::should_skip(&PathBuf::from(dir), true),
                "{dir} was wrongly skipped"
            );
        }
    }

    #[test]
    fn files_are_never_skipped_for_being_directories() {
        assert!(!ProjectFileIndexPolicy::should_skip(
            &PathBuf::from("node_modules"),
            false
        ));
    }

    #[test]
    fn a_file_inside_a_skipped_directory_is_skipped() {
        assert!(ProjectFileIndexPolicy::should_skip_file(&PathBuf::from(
            "node_modules/left-pad/index.js"
        )));
        assert!(ProjectFileIndexPolicy::should_skip_file(&PathBuf::from(
            "packages/app/node_modules/react/index.js"
        )));
    }

    #[test]
    fn a_file_named_like_a_skipped_directory_is_still_shown() {
        // Neither has a skipped *parent*: one is at the root, the other sits in
        // `src`. Only being inside `build/` would skip them.
        assert!(!ProjectFileIndexPolicy::should_skip_file(&PathBuf::from(
            "build"
        )));
        assert!(!ProjectFileIndexPolicy::should_skip_file(&PathBuf::from(
            "src/build.rs"
        )));
        assert!(ProjectFileIndexPolicy::should_skip_file(&PathBuf::from(
            "build/output.txt"
        )));
    }

    #[test]
    fn a_file_at_the_workspace_root_is_shown() {
        assert!(
            !ProjectFileIndexPolicy::should_skip_file(&PathBuf::from("README.md")),
            "a root-level file has no skipped parent"
        );
    }

    #[test]
    fn a_dot_directory_that_is_not_on_the_list_is_kept() {
        assert!(!ProjectFileIndexPolicy::should_skip(
            &PathBuf::from(".github"),
            true
        ));
        assert!(!ProjectFileIndexPolicy::should_skip(
            &PathBuf::from(".config"),
            true
        ));
    }
}
