//! What both transports do to a provider process: start it in a process group
//! of its own, keep the tail of what it writes to stderr, and stop it together
//! with everything it started.
//!
//! The group is the point. A provider runs tools as its own children — a
//! `cargo build`, a `sleep 200`, a `docker build` — and those inherit its
//! group. Signalling only the provider leaves the build running with nobody
//! left to report on it, which is what Stop used to do.

use std::time::Duration;

use tokio::process::{Child, Command};

/// How often a stopping group is checked for survivors.
const POLL: Duration = Duration::from_millis(20);

/// Start the command at the head of a new process group.
pub(crate) fn in_own_group(command: &mut Command) -> &mut Command {
    command.process_group(0)
}

/// A provider's process group, named by its leader's pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Group(libc::pid_t);

impl Group {
    /// The group a freshly spawned child leads, or `None` when it leads none.
    ///
    /// Checked rather than assumed. Signalling a group the child merely belongs
    /// to would signal whatever else is in it, and without
    /// [`in_own_group`] that is FlowX itself.
    pub(crate) fn led_by(child: &Child) -> Option<Self> {
        let pid = libc::pid_t::try_from(child.id()?).ok()?;
        // SAFETY: `getpgid` reads process state and has no memory effects.
        (pid > 0 && unsafe { libc::getpgid(pid) } == pid).then_some(Self(pid))
    }

    fn signal(self, signal: libc::c_int) {
        // SAFETY: `killpg` has no memory effects; a group that has already
        // emptied answers ESRCH, which is the outcome being asked for.
        unsafe {
            libc::killpg(self.0, signal);
        }
    }

    /// Whether anything is still in the group.
    fn is_alive(self) -> bool {
        // SAFETY: signal 0 only checks permission and existence.
        let answered = unsafe { libc::killpg(self.0, 0) };
        answered == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// End everything in the group: SIGTERM, a grace period, then SIGKILL.
    ///
    /// The grace is the group's, not only the leader's: a tool that outlives
    /// the provider by a moment is still given the chance to exit on SIGTERM
    /// before it is killed. The leader, when it is still held, is reaped along
    /// the way so its zombie does not read as a survivor.
    pub(crate) async fn terminate(self, mut leader: Option<&mut Child>, grace: Duration) {
        self.signal(libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if let Some(child) = leader.as_deref_mut() {
                let _ = child.try_wait();
            }
            if !self.is_alive() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        self.signal(libc::SIGKILL);
        if let Some(child) = leader {
            let _ = tokio::time::timeout(grace, child.wait()).await;
        }
    }
}

/// Stop one process politely: SIGTERM, a grace period, then SIGKILL.
///
/// For the end of a turn that finished. Nothing is running by then, and
/// anything the user deliberately left in the background is not FlowX's to
/// end, so the group is left alone.
pub(crate) async fn stop_leader(child: &mut Child, grace: Duration) {
    if let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: `kill` has no memory effects; the pid is our own unreaped
        // child, so it cannot have been reused.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(grace, child.wait()).await;
    }
}

/// Append to a byte tail that keeps at most `limit` bytes.
///
/// Bytes, not lines or text. A line reader stops at the first byte that is not
/// UTF-8 — and once the reader stops, the pipe closes and the provider's next
/// write to stderr fails — while output that only ever uses `\r` is one line
/// that grows without bound before any cap applies. The tail is kept rather
/// than the head because the last thing a CLI writes before it dies is the
/// reason it died.
pub(crate) fn keep_tail(buffer: &mut Vec<u8>, chunk: &[u8], limit: usize) {
    buffer.extend_from_slice(chunk);
    if buffer.len() <= limit {
        return;
    }
    let excess = buffer.len() - limit;
    buffer.drain(..excess);
    // Start on a character boundary, so decoding does not open the diagnosis
    // with a replacement mark for half a character.
    let partial = buffer
        .iter()
        .take(3)
        .take_while(|byte| *byte & 0xC0 == 0x80)
        .count();
    buffer.drain(..partial);
}

/// A captured tail, as text.
pub(crate) fn decode(buffer: &[u8]) -> String {
    String::from_utf8_lossy(buffer).into_owned()
}

/// Helpers for tests that start a real tool process.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::Duration;

    /// Kills a process when dropped, so a test that fails before stopping it
    /// does not leave a `sleep 60` behind.
    pub(crate) struct KillOnDrop(pub libc::pid_t);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if is_running(self.0) {
                // SAFETY: no memory effects.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                }
            }
        }
    }

    fn is_running(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Whether the process is gone within `timeout`.
    pub(crate) async fn gone_within(pid: libc::pid_t, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            while is_running(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_kept_within_its_limit() {
        let mut buffer = Vec::new();
        keep_tail(&mut buffer, b"banner\n", 64);
        for _ in 0..100 {
            // Carriage returns only: one endless "line" of progress output.
            keep_tail(&mut buffer, b"50%\r", 64);
        }
        keep_tail(&mut buffer, b"the real error\n", 64);
        assert!(buffer.len() <= 64, "{}", buffer.len());
        let text = decode(&buffer);
        assert!(text.ends_with("the real error\n"), "{text}");
        assert!(!text.contains("banner"), "the head was kept");
    }

    #[test]
    fn invalid_bytes_are_kept_and_decoded_lossily() {
        let mut buffer = Vec::new();
        keep_tail(&mut buffer, b"\xff\xfe broken\n", 64);
        keep_tail(&mut buffer, b"still reading\n", 64);
        let text = decode(&buffer);
        assert!(text.contains("still reading"), "{text}");
    }

    #[test]
    fn the_cut_never_starts_inside_a_character() {
        let mut buffer = Vec::new();
        keep_tail(&mut buffer, "é".repeat(40).as_bytes(), 33);
        assert!(std::str::from_utf8(&buffer).is_ok(), "{buffer:?}");
        assert!(buffer.len() <= 33);
    }
}
