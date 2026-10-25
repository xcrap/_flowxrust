//! What both transports do to a provider process: start it in a process group
//! of its own, keep the tail of what it writes to stderr, and stop it together
//! with everything it started.
//!
//! The group alone is not "everything it started". Both real CLIs move their
//! tools out of it: `claude` starts each `Bash` call in a session of its own,
//! and `codex app-server` gives each command a process group of its own, so
//! signalling the provider's group reached the provider and none of its tools.
//! Stopping works on a [`Tree`] instead: the provider, everything descended
//! from it, and every group and session one of those leads. The groups and
//! sessions are what still find a tool's children once the tool has exited and
//! they have been handed to launchd, where a walk by parent no longer reaches
//! them.
//!
//! A tree taken only when it is needed can be too late. Once the provider has
//! died or been killed, its tools belong to launchd, in sessions and groups no
//! walk from a dead pid reaches. A [`Watched`] tree is therefore kept current
//! while the provider runs, and every stop works from it: what it last saw,
//! joined with what the table says now.
//!
//! Quit cannot wait for a turn to wind down — GPUI gives its quit handlers a
//! tenth of a second before the process exits — so every transport registers
//! its provider for as long as it runs, and [`stop_all_now`] ends each
//! registered tree on the calling thread.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
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

    /// Everything the group's leader has started, as it stands now.
    ///
    /// Taken before the leader is asked to exit: once it has, its tools belong
    /// to launchd and only the groups and sessions recorded here find them.
    pub(crate) fn tree(self) -> Tree {
        let mut tree = Tree::new(self.0);
        tree.refresh(&process_table(), own_pid());
        tree
    }
}

/// One row of the process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub pid: libc::pid_t,
    pub ppid: libc::pid_t,
    pub pgid: libc::pid_t,
    pub sid: libc::pid_t,
}

/// A provider and everything it started.
///
/// A process belongs when it is the provider, a child of a member, or in a
/// group or session a member leads. Groups and sessions are kept while
/// anything is in them, because they are how an orphan is still found after
/// its parent has gone. An empty one is forgotten: nothing can join it again,
/// and its id is free for the next process to lead something unrelated.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    root: libc::pid_t,
    groups: BTreeSet<libc::pid_t>,
    sessions: BTreeSet<libc::pid_t>,
    /// Each member as it was when it joined, so a pid reused by an unrelated
    /// process — one in a different group or session — is not mistaken for it.
    members: BTreeMap<libc::pid_t, Entry>,
}

impl Tree {
    fn new(root: libc::pid_t) -> Self {
        Self {
            root,
            groups: BTreeSet::from([root]),
            sessions: BTreeSet::new(),
            members: BTreeMap::new(),
        }
    }

    /// Re-read membership from `table`.
    ///
    /// `parent` is FlowX's own pid. The provider counts only as FlowX's child,
    /// so a pid reused after it was reaped is never taken for it, and FlowX
    /// itself is never a member. A table missing a process (it exited, or it
    /// could not be read) drops it.
    pub(crate) fn refresh(&mut self, table: &[Entry], parent: libc::pid_t) {
        let current: BTreeMap<libc::pid_t, Entry> =
            table.iter().map(|entry| (entry.pid, *entry)).collect();
        self.members.retain(|pid, seen| {
            current
                .get(pid)
                .is_some_and(|now| now.pgid == seen.pgid && now.sid == seen.sid)
        });
        loop {
            let before = self.members.len();
            for entry in table {
                if entry.pid == parent || self.members.contains_key(&entry.pid) {
                    continue;
                }
                let belongs = (entry.pid == self.root && entry.ppid == parent)
                    || self.members.contains_key(&entry.ppid)
                    || self.groups.contains(&entry.pgid)
                    || self.sessions.contains(&entry.sid);
                if !belongs {
                    continue;
                }
                self.members.insert(entry.pid, *entry);
                if entry.pgid == entry.pid {
                    self.groups.insert(entry.pid);
                }
                if entry.sid == entry.pid {
                    self.sessions.insert(entry.pid);
                }
            }
            if self.members.len() == before {
                break;
            }
        }
        // A group or session ends with its last process, and a process can
        // join only one that exists, so an empty one stays empty until its id
        // is reused. Kept for as long as a watched tree lives, a reused id
        // would put an unrelated process in the tree, and Stop would end it.
        // A table that could not be read at all (FlowX is always in a real
        // one) says nothing about what is empty.
        if table.is_empty() {
            return;
        }
        self.groups
            .retain(|group| table.iter().any(|entry| entry.pgid == *group));
        self.sessions
            .retain(|session| table.iter().any(|entry| entry.sid == *session));
    }

    /// The pids that belong, as of the last refresh.
    #[cfg(test)]
    pub(crate) fn members(&self) -> Vec<libc::pid_t> {
        self.members.keys().copied().collect()
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn refresh_now(&mut self) {
        self.refresh(&process_table(), own_pid());
    }

    /// Signal every group a member leads and every member by pid, which also
    /// reaches a member sitting in a group nobody in the tree leads.
    fn signal(&self, signal: libc::c_int) {
        let own = own_pid();
        // SAFETY: `getpgid(0)` reads FlowX's own group.
        let own_group = unsafe { libc::getpgid(0) };
        for group in &self.groups {
            if *group > 1 && *group != own_group {
                // SAFETY: `killpg` has no memory effects; a group that has
                // already emptied answers ESRCH, the outcome being asked for.
                unsafe {
                    libc::killpg(*group, signal);
                }
            }
        }
        for pid in self.members.keys() {
            if *pid > 1 && *pid != own {
                // SAFETY: as above, for one process.
                unsafe {
                    libc::kill(*pid, signal);
                }
            }
        }
    }

    /// End the tree: SIGTERM, a grace period, then SIGKILL.
    ///
    /// The grace is the tree's, not only the leader's: a tool that outlives
    /// the provider by a moment is still given the chance to exit on SIGTERM
    /// before it is killed. The leader, when it is still held, is reaped along
    /// the way so its zombie does not read as a survivor.
    pub(crate) async fn terminate(mut self, mut leader: Option<&mut Child>, grace: Duration) {
        self.refresh_now();
        self.signal(libc::SIGTERM);
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if let Some(child) = leader.as_deref_mut() {
                let _ = child.try_wait();
            }
            self.refresh_now();
            if self.is_empty() {
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

/// A provider's tree, remembered from the moment it starts and kept current
/// while it runs.
///
/// Shared, because the transport refreshes it as the provider works while
/// Stop and Quit read it from elsewhere. A refresh only adds what the table
/// shows and drops what has gone, so the record is always the union of what
/// was seen and what is there now.
#[derive(Debug, Clone)]
pub(crate) struct Watched(Arc<Mutex<Tree>>);

impl Watched {
    /// Start watching what `group`'s leader starts, from what it has now.
    pub(crate) fn new(group: Group) -> Self {
        Self(Arc::new(Mutex::new(group.tree())))
    }

    /// Re-read the tree from the process table.
    ///
    /// The table is read before the lock is taken, so a stop reading the tree
    /// never waits on the table.
    pub(crate) fn refresh(&self) {
        let table = process_table();
        self.refresh_from(&table, own_pid());
    }

    fn refresh_from(&self, table: &[Entry], parent: libc::pid_t) {
        self.lock().refresh(table, parent);
    }

    /// The tree as of now, to stop.
    pub(crate) fn snapshot(&self) -> Tree {
        self.refresh();
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tree> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// End several trees on this thread, sharing one grace period.
fn end_blocking(trees: &mut [Tree], grace: Duration) {
    for tree in trees.iter() {
        tree.signal(libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + grace;
    loop {
        std::thread::sleep(POLL);
        let table = process_table();
        let own = own_pid();
        for tree in trees.iter_mut() {
            tree.refresh(&table, own);
        }
        if trees.iter().all(Tree::is_empty) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    for tree in trees.iter() {
        tree.signal(libc::SIGKILL);
    }
}

/// The providers running now, by the pid that leads each one's group, each
/// with its tree as last seen.
static RUNNING: Mutex<BTreeMap<libc::pid_t, Watched>> = Mutex::new(BTreeMap::new());

/// A provider's entry in the list Quit ends. Dropping it removes the entry.
///
/// A transport holds one from spawn until it closes or terminates. A closed
/// transport's leftovers — a dev server started on purpose — are not FlowX's
/// to end, at Quit any more than at the end of a turn.
#[derive(Debug)]
pub(crate) struct Running(libc::pid_t);

impl Running {
    /// List a provider with the tree its transport keeps current, so Quit
    /// still finds a tool whose provider has already gone.
    pub(crate) fn watching(group: Group, tree: Watched) -> Self {
        running().insert(group.0, tree);
        Self(group.0)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        running().remove(&self.0);
    }
}

fn running() -> std::sync::MutexGuard<'static, BTreeMap<libc::pid_t, Watched>> {
    RUNNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How long Quit gives the running providers to exit on SIGTERM.
pub const QUIT_GRACE: Duration = Duration::from_millis(500);

/// End every running provider and everything it started, on this thread.
///
/// For Quit, which cannot wait for a turn's own teardown: SIGTERM to every
/// tree at once, up to `grace` for them to exit, then SIGKILL.
pub fn stop_all_now(grace: Duration) {
    // Taken out first, so the list is not locked while the trees are read.
    let watched: Vec<Watched> = running().values().cloned().collect();
    stop_now(&watched, grace);
}

/// [`stop_all_now`] for the `watched` providers.
fn stop_now(watched: &[Watched], grace: Duration) {
    if watched.is_empty() {
        return;
    }
    let table = process_table();
    let own = own_pid();
    let mut trees: Vec<Tree> = watched
        .iter()
        .map(|tree| {
            tree.refresh_from(&table, own);
            tree.lock().clone()
        })
        .collect();
    end_blocking(&mut trees, grace);
}

fn own_pid() -> libc::pid_t {
    // SAFETY: `getpid` cannot fail and has no memory effects.
    unsafe { libc::getpid() }
}

/// Every live process this user can read, with its parent, group and session.
///
/// Zombies are left out: one is already dead, and counting it would read as a
/// survivor until whoever owns it reaps it.
#[cfg(target_os = "macos")]
pub(crate) fn process_table() -> Vec<Entry> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for whatever starts between the two calls.
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let Ok(bytes) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
        return Vec::new();
    };
    // SAFETY: the buffer is `bytes` long and lives across the call.
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    let Ok(filled) = usize::try_from(filled) else {
        return Vec::new();
    };
    pids.truncate(filled.min(pids.len()));
    pids.into_iter()
        .filter(|pid| *pid > 0)
        .filter_map(table_entry)
        .collect()
}

#[cfg(target_os = "macos")]
fn table_entry(pid: libc::pid_t) -> Option<Entry> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdshortinfo>()).ok()?;
    // SAFETY: the buffer is one `proc_bsdshortinfo`, which is what this flavor
    // writes, and `size` says so.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    // SAFETY: `proc_pidinfo` filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    if info.pbsi_status == libc::SZOMB {
        return None;
    }
    // SAFETY: `getsid` reads process state and has no memory effects.
    let sid = unsafe { libc::getsid(pid) };
    Some(Entry {
        pid,
        ppid: libc::pid_t::try_from(info.pbsi_ppid).ok()?,
        pgid: libc::pid_t::try_from(info.pbsi_pgid).ok()?,
        sid,
    })
}

/// Elsewhere there is no table to read: only the provider's own group is
/// signalled, and only with SIGTERM.
#[cfg(not(target_os = "macos"))]
pub(crate) fn process_table() -> Vec<Entry> {
    Vec::new()
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

    fn entry(pid: i32, ppid: i32, pgid: i32, sid: i32) -> Entry {
        Entry {
            pid,
            ppid,
            pgid,
            sid,
        }
    }

    /// FlowX is 100, in its own group and session.
    const FLOWX: i32 = 100;

    fn tree_over(table: &[Entry]) -> Tree {
        let mut tree = Tree::new(200);
        tree.refresh(table, FLOWX);
        tree
    }

    #[test]
    fn a_tool_in_its_own_session_or_group_is_part_of_the_tree() {
        // What the real CLIs do: Claude's `Bash` gets a session of its own,
        // Codex's command a group of its own. Neither is in the provider's
        // group, which is all the old Stop signalled.
        let table = [
            entry(FLOWX, 1, FLOWX, FLOWX),
            entry(200, FLOWX, 200, FLOWX),
            entry(300, 200, 300, 300),
            entry(301, 300, 300, 300),
            entry(400, 200, 400, FLOWX),
            entry(401, 400, 400, FLOWX),
            entry(500, 1, 500, 500),
        ];
        assert_eq!(tree_over(&table).members(), [200, 300, 301, 400, 401]);
    }

    #[test]
    fn an_orphan_is_still_found_by_the_session_or_group_it_was_in() {
        let mut tree = tree_over(&[
            entry(FLOWX, 1, FLOWX, FLOWX),
            entry(200, FLOWX, 200, FLOWX),
            entry(300, 200, 300, 300),
            entry(301, 300, 300, 300),
            entry(400, 200, 400, FLOWX),
            entry(401, 400, 400, FLOWX),
        ]);
        // The provider and both tools have exited; their children now belong
        // to launchd, and a walk by parent reaches neither.
        tree.refresh(
            &[
                entry(FLOWX, 1, FLOWX, FLOWX),
                entry(301, 1, 300, 300),
                entry(401, 1, 400, FLOWX),
            ],
            FLOWX,
        );
        assert_eq!(tree.members(), [301, 401]);
    }

    #[test]
    fn flowx_and_its_own_group_and_session_are_never_members() {
        // A tool that joined FlowX's group still dies by pid, but the group
        // itself, which holds FlowX, is never one of the tree's.
        let tree = tree_over(&[
            entry(FLOWX, 1, FLOWX, FLOWX),
            entry(200, FLOWX, 200, FLOWX),
            entry(300, 200, FLOWX, FLOWX),
            entry(600, FLOWX, FLOWX, FLOWX),
        ]);
        assert_eq!(tree.members(), [200, 300]);
        assert!(!tree.groups.contains(&FLOWX));
        assert!(!tree.sessions.contains(&FLOWX));
    }

    #[test]
    fn a_reused_pid_is_not_taken_for_the_provider_or_a_member() {
        // The provider was reaped and its pid handed to something else.
        let tree = tree_over(&[entry(FLOWX, 1, FLOWX, FLOWX), entry(200, 1, 777, 777)]);
        assert!(tree.members().is_empty());

        let mut tree = tree_over(&[
            entry(FLOWX, 1, FLOWX, FLOWX),
            entry(200, FLOWX, 200, FLOWX),
            entry(300, 200, 200, FLOWX),
        ]);
        // 300 exited and its pid now names an unrelated process.
        tree.refresh(
            &[
                entry(FLOWX, 1, FLOWX, FLOWX),
                entry(200, FLOWX, 200, FLOWX),
                entry(300, 1, 888, 888),
            ],
            FLOWX,
        );
        assert_eq!(tree.members(), [200]);
    }

    #[test]
    fn an_emptied_session_is_forgotten_so_its_reused_id_is_never_claimed() {
        // A watched tree lives as long as its provider, hours if need be. A
        // session kept after its last process had gone made the next process
        // to lead one under that id a member, and Stop would have ended it.
        let mut tree = tree_over(&[
            entry(FLOWX, 1, FLOWX, FLOWX),
            entry(200, FLOWX, 200, FLOWX),
            entry(300, 200, 300, 300),
        ]);
        assert_eq!(tree.members(), [200, 300]);
        // The tool exits, and with it its session.
        tree.refresh(
            &[entry(FLOWX, 1, FLOWX, FLOWX), entry(200, FLOWX, 200, FLOWX)],
            FLOWX,
        );
        assert_eq!(tree.members(), [200]);
        // An unrelated program is handed the same id and leads a session.
        tree.refresh(
            &[
                entry(FLOWX, 1, FLOWX, FLOWX),
                entry(200, FLOWX, 200, FLOWX),
                entry(300, 1, 300, 300),
            ],
            FLOWX,
        );
        assert_eq!(tree.members(), [200]);
    }

    #[test]
    fn a_table_that_could_not_be_read_forgets_nothing() {
        // Where there is no table the provider's own group is still the one
        // signalled, so an empty read must not empty the record.
        let mut tree = tree_over(&[entry(FLOWX, 1, FLOWX, FLOWX), entry(200, FLOWX, 200, FLOWX)]);
        tree.refresh(&[], FLOWX);
        assert!(tree.groups.contains(&200));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_process_table_reads_this_process() {
        let own = own_pid();
        let table = process_table();
        let this = table
            .iter()
            .find(|entry| entry.pid == own)
            .expect("this process is listed");
        // SAFETY: no memory effects.
        assert_eq!(this.pgid, unsafe { libc::getpgid(0) });
        assert_eq!(this.sid, unsafe { libc::getsid(0) });
    }

    /// A provider that starts its tools the way the real CLIs do: one in a
    /// session of its own running a child of its own, one in a group of its
    /// own. It prints their pids and waits.
    #[cfg(target_os = "macos")]
    const PROVIDER: &str = r#"
import os, subprocess, sys, time
shell = subprocess.Popen(["sh", "-c", "sleep 60 & echo $!; wait"],
                         start_new_session=True, stdout=subprocess.PIPE)
inner = shell.stdout.readline().decode().strip()
command = subprocess.Popen(["sleep", "60"], preexec_fn=os.setpgrp)
print(shell.pid, inner, command.pid, flush=True)
time.sleep(60)
"#;

    /// Start [`PROVIDER`] in a group of its own, and read its tools' pids.
    #[cfg(target_os = "macos")]
    async fn provider_with_tools() -> Option<(Child, Group, Vec<libc::pid_t>)> {
        use tokio::io::AsyncBufReadExt;
        let mut command = Command::new("python3");
        command
            .args(["-c", PROVIDER])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        in_own_group(&mut command);
        let Ok(mut child) = command.spawn() else {
            eprintln!("skipping: python3 is not on PATH");
            return None;
        };
        let group = Group::led_by(&child).expect("leads its group");
        let stdout = child.stdout.take().expect("piped");
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new(stdout).read_line(&mut line),
        )
        .await
        .expect("the tools started")
        .expect("read");
        let pids = line
            .split_whitespace()
            .map(|pid| pid.parse().expect("a pid"))
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 3, "{line:?}");
        Some((child, group, pids))
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn terminating_ends_tools_outside_the_providers_group() {
        let Some((mut child, group, pids)) = provider_with_tools().await else {
            return;
        };
        let _reap: Vec<_> = pids.iter().map(|pid| testing::KillOnDrop(*pid)).collect();
        group
            .tree()
            .terminate(Some(&mut child), Duration::from_millis(500))
            .await;
        for pid in pids {
            assert!(
                testing::gone_within(pid, Duration::from_secs(5)).await,
                "tool {pid} outlived Stop"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn tools_are_ended_after_the_provider_has_already_exited() {
        // Claude is given a moment to exit on its own before the tree is
        // stopped. Once it has, its tools belong to launchd: only the tree
        // taken beforehand still leads to them.
        let Some((mut child, group, pids)) = provider_with_tools().await else {
            return;
        };
        let _reap: Vec<_> = pids.iter().map(|pid| testing::KillOnDrop(*pid)).collect();
        let tree = group.tree();
        let _ = child.start_kill();
        let _ = child.wait().await;
        tree.terminate(None, Duration::from_millis(500)).await;
        for pid in pids {
            assert!(
                testing::gone_within(pid, Duration::from_secs(5)).await,
                "tool {pid} outlived its provider and Stop"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn quitting_ends_a_running_provider_on_the_calling_thread() {
        // Quit cannot await a turn's teardown, so this one blocks instead.
        let Some((mut child, group, pids)) = provider_with_tools().await else {
            return;
        };
        let _reap: Vec<_> = pids.iter().map(|pid| testing::KillOnDrop(*pid)).collect();
        let registration = Running::watching(group, Watched::new(group));
        let watched = running().get(&group.0).cloned();
        let watched = watched.expect("a running provider is listed");
        // Only this provider: the list is shared by every test in the process.
        stop_now(&[watched], Duration::from_millis(500));
        for pid in pids {
            assert!(
                testing::gone_within(pid, Duration::from_secs(1)).await,
                "tool {pid} outlived Quit"
            );
        }
        let _ = child.wait().await;
        drop(registration);
        assert!(
            !running().contains_key(&group.0),
            "a closed provider is listed"
        );
    }

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
