//! Reading and writing JSON with a recovery copy.
//!
//! A corrupt preferences file should not cost the user their projects. Every
//! write is atomic and leaves a `.backup` sibling, and every read tries both.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// The suffix of the recovery copy.
pub const BACKUP_SUFFIX: &str = "backup";

#[derive(Debug, Error)]
pub enum ReadError {
    #[error("neither {path} nor its backup could be read")]
    Unreadable { path: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("could not create the parent directory for {path}")]
    NoParent { path: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

/// The path of the recovery copy for a file.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

/// Read JSON, falling back to the recovery copy.
///
/// Both a failed *read* and a failed *parse* fall back. The second is the case
/// this exists for: a write interrupted mid-file leaves a truncated primary
/// that opens perfectly and then does not parse, so a fallback that only
/// triggers on `read` would never fire for the damage it was added to survive.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ReadError> {
    let unreadable = || ReadError::Unreadable {
        path: path.display().to_string(),
    };

    if let Ok(bytes) = std::fs::read(path) {
        if let Ok(value) = serde_json::from_slice(&bytes) {
            return Ok(value);
        }
        tracing::warn!(path = %path.display(), "primary file did not parse; trying the recovery copy");
    }

    match std::fs::read(backup_path(path)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| unreadable()),
        Err(_) => Err(unreadable()),
    }
}

/// Whether a file has a recovery copy.
pub fn has_backup(path: &Path) -> bool {
    backup_path(path).exists()
}

/// Write JSON atomically, then refresh the recovery copy.
///
/// The sequence matters: the primary must land before the copy is replaced, so a
/// crash between the two leaves the previous good copy in place rather than
/// losing both.
pub fn write_json_atomically<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), WriteError> {
    let parent = path.parent().ok_or_else(|| WriteError::NoParent {
        path: path.display().to_string(),
    })?;
    super::paths::create_private_dir(parent)?;

    let bytes = serde_json::to_vec_pretty(value)?;
    let temp = temp_sibling(path);
    std::fs::write(&temp, &bytes)?;
    std::fs::rename(&temp, path)?;
    std::fs::write(backup_path(path), &bytes)?;
    Ok(())
}

/// A sibling temporary file, so the rename stays on one filesystem and is
/// therefore atomic.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Remove a file and its recovery copy.
///
/// Both are removed together so a later write cannot resurrect stale data.
pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match std::fs::remove_file(backup_path(path)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn sample(name: &str, count: u32) -> Sample {
        Sample {
            name: name.into(),
            count,
        }
    }

    #[test]
    fn a_written_file_reads_back_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("flowx", 3)).unwrap();
        assert_eq!(read_json::<Sample>(&path).unwrap(), sample("flowx", 3));
    }

    #[test]
    fn writing_leaves_a_recovery_copy() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("first", 1)).unwrap();
        assert!(has_backup(&path));

        write_json_atomically(&path, &sample("second", 2)).unwrap();
        // The copy tracks the latest write, not the first.
        assert_eq!(
            read_json::<Sample>(&backup_path(&path)).unwrap(),
            sample("second", 2)
        );
    }

    #[test]
    fn a_corrupt_primary_falls_back_to_the_backup() {
        // The case this whole mechanism exists for.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("good", 1)).unwrap();

        std::fs::write(&path, b"{ truncated").unwrap();
        assert_eq!(
            read_json::<Sample>(&path).unwrap(),
            sample("good", 1),
            "a corrupt primary lost the data"
        );
    }

    #[test]
    fn a_truncated_primary_falls_back_to_the_backup() {
        // The interrupted-write case: the primary opens, and then does not parse.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("intact", 1)).unwrap();
        std::fs::write(&path, b"{\"name\": \"half").unwrap();
        assert_eq!(read_json::<Sample>(&path).unwrap(), sample("intact", 1));
    }

    #[test]
    fn an_empty_primary_falls_back_to_the_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("intact", 1)).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read_json::<Sample>(&path).unwrap(), sample("intact", 1));
    }

    #[test]
    fn a_missing_file_with_no_backup_reports_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("absent.json");
        assert!(matches!(
            read_json::<Sample>(&path),
            Err(ReadError::Unreadable { .. })
        ));
    }

    #[test]
    fn a_backup_that_also_fails_reports_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        std::fs::write(&path, b"nope").unwrap();
        std::fs::write(backup_path(&path), b"also nope").unwrap();
        assert!(read_json::<Sample>(&path).is_err());
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b/c/state.json");
        write_json_atomically(&path, &sample("nested", 1)).unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn writing_leaves_no_temporary_file_behind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("clean", 1)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a temporary file was not renamed away"
        );
    }

    #[test]
    fn removal_takes_the_backup_too() {
        // Leaving the copy behind would let a later write resurrect stale data.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("x", 1)).unwrap();
        remove(&path).unwrap();
        assert!(!path.exists());
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn removing_an_absent_file_is_harmless() {
        let temp = tempfile::tempdir().unwrap();
        remove(&temp.path().join("never-existed.json")).unwrap();
    }

    #[test]
    fn the_output_is_readable_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("pretty", 1)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains('\n'), "the file should be pretty-printed");
        assert!(text.contains("\"name\""));
    }
}
