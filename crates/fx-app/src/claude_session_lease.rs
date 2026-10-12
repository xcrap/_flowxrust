//! One writer per Claude session, shared with the Swift app's lease directory.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(crate) struct ClaudeSessionLease {
    _file: File,
}

impl ClaudeSessionLease {
    pub fn acquire(root: &Path, session: &str) -> Result<Self, String> {
        let id = uuid::Uuid::parse_str(session.trim())
            .map_err(|_| "The saved Claude session ID is invalid.".to_string())?;
        std::fs::create_dir_all(root).map_err(unavailable)?;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(unavailable)?;
        let metadata = directory.metadata().map_err(unavailable)?;
        // SAFETY: geteuid takes no pointers and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if !metadata.is_dir() || metadata.uid() != uid {
            return Err("FlowX cannot safely reserve this Claude session.".into());
        }
        directory
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(unavailable)?;
        let name = CString::new(format!("{id}.lock")).unwrap();
        // SAFETY: the directory descriptor is owned and open, and name is a
        // NUL-terminated single filename. O_NOFOLLOW rejects redirected leases.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(unavailable(std::io::Error::last_os_error()));
        }
        // SAFETY: openat returned a new descriptor; File is its sole owner.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata().map_err(unavailable)?;
        if !metadata.is_file() || metadata.uid() != uid || metadata.nlink() != 1 {
            return Err("FlowX cannot safely reserve this Claude session.".into());
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(unavailable)?;
        // SAFETY: the file descriptor remains valid while the lease owns File.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err("This Claude session is already running in FlowX. Wait for that turn to finish before replying.".into());
            }
            return Err(unavailable(error));
        }
        Ok(Self { _file: file })
    }
}

fn unavailable(error: std::io::Error) -> String {
    format!("FlowX could not reserve the Claude session: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_excludes_another_writer_and_is_released_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let first = ClaudeSessionLease::acquire(directory.path(), &id).unwrap();
        assert!(ClaudeSessionLease::acquire(directory.path(), &id).is_err());
        drop(first);
        assert!(ClaudeSessionLease::acquire(directory.path(), &id).is_ok());
    }

    #[test]
    fn a_lease_rejects_symlinks_and_invalid_ids() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("locks");
        std::fs::create_dir(&root).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(ClaudeSessionLease::acquire(&alias, &id).is_err());
        let file = directory.path().join("file");
        std::fs::write(&file, "kept").unwrap();
        std::os::unix::fs::symlink(&file, root.join(format!("{id}.lock"))).unwrap();
        assert!(ClaudeSessionLease::acquire(&root, &id).is_err());
        assert!(ClaudeSessionLease::acquire(&root, "../../escape").is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "kept");
    }
}
