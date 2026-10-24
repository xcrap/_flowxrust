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
/// losing both. The copy is replaced the same way as the primary, never
/// rewritten in place: truncating it first would leave a moment, during the
/// write that refreshes it, at which neither file holds a whole snapshot.
pub fn write_json_atomically<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), WriteError> {
    write_atomically(path, &serde_json::to_vec_pretty(value)?)
}

/// Write bytes that are already encoded, with the same sequence and recovery
/// copy as [`write_json_atomically`].
pub fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    write_bytes(path, bytes, &flush_to_disk)
}

/// Flushes one open file, or one directory, to stable storage.
type Flush<'a> = dyn Fn(&std::fs::File) -> std::io::Result<()> + 'a;

fn write_bytes(path: &Path, bytes: &[u8], flush: &Flush<'_>) -> Result<(), WriteError> {
    let parent = path.parent().ok_or_else(|| WriteError::NoParent {
        path: path.display().to_string(),
    })?;
    super::paths::create_private_dir(parent)?;
    replace(path, parent, bytes, flush)?;
    replace(&backup_path(path), parent, bytes, flush)?;
    // The renames are changes to the directory, which has its own flush. One
    // after both is enough: each name already points at flushed data, so
    // whichever renames a power cut keeps, both names hold a whole snapshot.
    flush(&std::fs::File::open(parent)?)?;
    Ok(())
}

/// Replace one file so that it is never seen partly written, even after a
/// power cut. The caller flushes the directory.
///
/// The temporary sibling keeps the rename on one filesystem, and therefore
/// atomic. Its name is unique, so two instances saving at once cannot truncate
/// each other's half-written file, and it is created owner-only, which the
/// rename carries over.
fn replace(path: &Path, directory: &Path, bytes: &[u8], flush: &Flush<'_>) -> std::io::Result<()> {
    use std::io::Write;

    let mut prefix = path.file_name().unwrap_or_default().to_os_string();
    prefix.push(".");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut temp = builder.tempfile_in(directory)?;
    temp.write_all(bytes)?;
    // The data reaches the disk before a name points at it. Otherwise a power
    // cut can keep the rename and lose the data, leaving an empty file.
    flush(temp.as_file())?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Flush a file's data, or a directory's entries, to stable storage.
///
/// On macOS `fsync` only hands the data to the drive, which can hold it in a
/// volatile cache through a power cut; `F_FULLFSYNC` makes the drive write it
/// out. Some filesystems, network volumes among them, do not support that, so
/// `fsync` is the fallback — the same order SQLite uses.
fn flush_to_disk(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let descriptor = file.as_raw_fd();
        // SAFETY: `descriptor` belongs to `file`, which is open for both calls.
        if unsafe { libc::fcntl(descriptor, libc::F_FULLFSYNC) } != -1 {
            return Ok(());
        }
        // SAFETY: as above.
        if unsafe { libc::fsync(descriptor) } == 0 {
            return Ok(());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_all()
    }
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
    fn the_recovery_copy_is_replaced_rather_than_rewritten_in_place() {
        // Rewriting it in place truncates it first, so a crash during that
        // write could leave a half-written primary *and* a half-written copy.
        // A link to the old copy shows which happened: a replaced file keeps
        // its old contents, a rewritten one does not.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("first", 1)).unwrap();
        let old_copy = temp.path().join("old-copy");
        std::fs::hard_link(backup_path(&path), &old_copy).unwrap();

        write_json_atomically(&path, &sample("second", 2)).unwrap();
        assert_eq!(read_json::<Sample>(&old_copy).unwrap(), sample("first", 1));
        assert_eq!(
            read_json::<Sample>(&backup_path(&path)).unwrap(),
            sample("second", 2)
        );
    }

    #[test]
    fn another_instances_temporary_file_is_left_alone() {
        // A fixed temporary name meant two instances saving together truncated
        // each other's file, and one renamed the other's half-written bytes.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let theirs = temp.path().join("state.json.tmp");
        std::fs::write(&theirs, b"another instance, mid-write").unwrap();

        write_json_atomically(&path, &sample("mine", 1)).unwrap();
        assert_eq!(
            std::fs::read(&theirs).unwrap(),
            b"another instance, mid-write"
        );
        assert_eq!(read_json::<Sample>(&path).unwrap(), sample("mine", 1));
    }

    #[cfg(unix)]
    #[test]
    fn written_files_and_the_directories_made_for_them_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("a/b/c/state.json");
        write_json_atomically(&path, &sample("private", 1)).unwrap();

        for directory in ["a", "a/b", "a/b/c"] {
            assert_eq!(mode(&temp.path().join(directory)), 0o700, "{directory}");
        }
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&backup_path(&path)), 0o600);
    }

    #[test]
    fn each_copy_reaches_the_disk_before_its_rename_and_the_rename_after() {
        use std::cell::RefCell;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        write_json_atomically(&path, &sample("old", 1)).unwrap();
        let current = |path: &Path| read_json::<Sample>(path).unwrap().name;

        // What was on disk at each flush: whether the flushed handle was the
        // directory, then the primary's and the copy's contents at the time.
        // Each copy's data is flushed before its rename, and the directory
        // once both renames are done.
        let flushes = RefCell::new(Vec::new());
        let record = |file: &std::fs::File| {
            flushes.borrow_mut().push((
                file.metadata()?.is_dir(),
                current(&path),
                current(&backup_path(&path)),
            ));
            Ok(())
        };
        let bytes = serde_json::to_vec(&sample("new", 2)).unwrap();
        write_bytes(&path, &bytes, &record).unwrap();

        let step = |dir: bool, primary: &str, copy: &str| (dir, primary.into(), copy.into());
        assert_eq!(
            flushes.into_inner(),
            vec![
                step(false, "old", "old"),
                step(false, "new", "old"),
                step(true, "new", "new"),
            ]
        );
    }

    #[test]
    fn the_real_flush_succeeds_for_a_file_and_its_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        std::fs::write(&path, b"{}").unwrap();
        flush_to_disk(&std::fs::File::open(&path).unwrap()).unwrap();
        flush_to_disk(&std::fs::File::open(temp.path()).unwrap()).unwrap();
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
