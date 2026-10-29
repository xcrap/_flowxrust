//! One poller for the active project's working tree.
//!
//! The title bar's change count and the inspector show the same snapshot.
//! Each used to poll on its own five-second timer, so every tick ran
//! `git status` and both numstat passes twice. Both now observe this entity,
//! which reads once per tick and notifies only when the read differs from the
//! last one: an unchanged tree costs no repaint and no diff reload anywhere.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AppContext, Context, Entity, Task};

use super::repository::{self, Snapshot};
use crate::state::AppState;

/// How often the working tree is read while nothing else asks for a read.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct MonitorHandle(Entity<RepositoryMonitor>);
impl gpui::Global for MonitorHandle {}

pub(crate) struct RepositoryMonitor {
    state: Entity<AppState>,
    project: Option<String>,
    /// Bumped on every project switch, so a read that outlives its project is
    /// recognisably stale when it lands.
    generation: u64,
    snapshot: Option<Arc<Snapshot>>,
    error: Option<String>,
    loading: bool,
    /// A refresh requested while a read was running. That read may predate
    /// whatever prompted the request — a commit, say — so another follows.
    queued: bool,
    was_busy: bool,
    /// Reads started, for tests that count work.
    reads: u64,
    read_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
}

/// The app's one monitor for `state`, created on first use.
pub(crate) fn shared(state: &Entity<AppState>, cx: &mut App) -> Entity<RepositoryMonitor> {
    if let Some(handle) = cx.try_global::<MonitorHandle>()
        && handle.0.read(cx).state == *state
    {
        return handle.0.clone();
    }
    let monitor = cx.new(|_| RepositoryMonitor::new(state.clone()));
    cx.set_global(MonitorHandle(monitor.clone()));
    monitor
}

impl RepositoryMonitor {
    pub(crate) fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            project: None,
            generation: 0,
            snapshot: None,
            error: None,
            loading: false,
            queued: false,
            was_busy: false,
            reads: 0,
            read_task: None,
            poll_task: None,
        }
    }

    pub(crate) fn snapshot(&self) -> Option<&Arc<Snapshot>> {
        self.snapshot.as_ref()
    }

    /// Why the last read failed. The snapshot before it is kept, because the
    /// inspector can still label it stale; the title bar hides instead.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn reads(&self) -> u64 {
        self.reads
    }

    /// Follow the active project and the end of a turn. Cheap enough to call
    /// from every render.
    pub(crate) fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let project = state.active_workspace_root();
        let busy = state.is_busy();
        if self.project != project {
            self.project = project;
            self.generation += 1;
            self.read_task = None;
            self.loading = false;
            self.queued = false;
            self.snapshot = None;
            self.error = None;
            self.refresh(cx);
            cx.notify();
        } else if self.was_busy && !busy {
            // A finished turn has usually just written files.
            self.refresh(cx);
        }
        self.was_busy = busy;
        if self.poll_task.is_none() {
            self.poll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(POLL_INTERVAL).await;
                    let polled = this.update(cx, |this, cx| {
                        if !this.loading {
                            this.refresh(cx);
                        }
                    });
                    if polled.is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// Start a read, or queue one behind the read in flight.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.project.clone() else {
            return;
        };
        if self.loading {
            self.queued = true;
            return;
        }
        self.loading = true;
        self.reads += 1;
        let generation = self.generation;
        let workspace = self
            .state
            .read(cx)
            .active_agent()
            .and_then(|agent| agent.workspace);
        let job = repository::spawn_git(move || async move {
            if let Some(workspace) = workspace {
                fx_core::git::worktree::validate(&workspace).await?;
            }
            repository::changes_snapshot(&PathBuf::from(root)).await
        });
        self.read_task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_git(job).await;
            let _ = this.update(cx, |this, cx| this.complete_read(generation, result, cx));
        }));
        if self.snapshot.is_none() {
            // "Reading…" only matters before there is anything to show.
            cx.notify();
        }
    }

    pub(crate) fn complete_read(
        &mut self,
        generation: u64,
        result: Result<Snapshot, String>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        self.loading = false;
        let changed = match result {
            Ok(snapshot) => {
                let same = self
                    .snapshot
                    .as_ref()
                    .is_some_and(|current| current.same_as(&snapshot));
                let recovered = self.error.take().is_some();
                if !same {
                    self.snapshot = Some(Arc::new(snapshot));
                }
                !same || recovered
            }
            Err(error) => {
                let changed = self.error.as_deref() != Some(error.as_str());
                self.error = Some(error);
                changed
            }
        };
        if changed {
            cx.notify();
        }
        if std::mem::take(&mut self.queued) {
            self.refresh(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new(fx_core::git::GIT)
            .args(args)
            .current_dir(root)
            .envs(fx_core::git::hardened_environment())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "flowx-test@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "FlowX test"]);
        std::fs::write(temp.path().join("sample.txt"), "original\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        temp
    }

    pub(crate) fn snapshot(root: &std::path::Path) -> Snapshot {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(repository::changes_snapshot(root))
            .unwrap()
    }

    fn state(test: &mut gpui::TestAppContext, path: &std::path::Path) -> Entity<AppState> {
        test.update(|cx| cx.new(|cx| AppState::restore(fx_store::AppPaths::at(path), cx)))
    }

    #[test]
    fn a_switched_project_rejects_late_reads_and_failures_keep_the_last_snapshot() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "edited\n").unwrap();
        let other = repo();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        state.update(&mut test, |state, _| {
            state.add_project(&root.path().to_string_lossy());
        });
        let monitor = test.update(|cx| cx.new(|_| RepositoryMonitor::new(state.clone())));
        let generation = monitor.update(&mut test, |monitor, cx| {
            monitor.sync(cx);
            monitor.complete_read(monitor.generation, Ok(snapshot(root.path())), cx);
            assert!(monitor.snapshot.is_some());
            monitor.generation
        });
        state.update(&mut test, |state, _| {
            state.add_project(&other.path().to_string_lossy());
        });
        monitor.update(&mut test, |monitor, cx| {
            monitor.sync(cx);
            assert!(monitor.snapshot.is_none());
            monitor.complete_read(generation, Ok(snapshot(root.path())), cx);
            assert!(
                monitor.snapshot.is_none(),
                "the previous project's read must be ignored"
            );
            monitor.complete_read(monitor.generation, Ok(snapshot(other.path())), cx);
            let kept = monitor.snapshot.clone().unwrap();
            monitor.complete_read(monitor.generation, Err("read failed".into()), cx);
            assert_eq!(monitor.error(), Some("read failed"));
            assert!(Arc::ptr_eq(monitor.snapshot.as_ref().unwrap(), &kept));
        });
    }

    #[gpui::test]
    fn an_identical_read_notifies_nobody(cx: &mut gpui::TestAppContext) {
        let root = repo();
        let data = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| AppState::restore(fx_store::AppPaths::at(data.path()), cx));
        let monitor = cx.new(|_| RepositoryMonitor::new(state));
        let notified = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = notified.clone();
        let _subscription =
            cx.update(|cx| cx.observe(&monitor, move |_, _| count.set(count.get() + 1)));
        let first = snapshot(root.path());
        monitor.update(cx, |monitor, cx| {
            monitor.complete_read(0, Ok(first.clone()), cx);
        });
        cx.run_until_parked();
        assert_eq!(notified.get(), 1);
        let before = monitor.read_with(cx, |monitor, _| monitor.snapshot.clone().unwrap());
        monitor.update(cx, |monitor, cx| monitor.complete_read(0, Ok(first), cx));
        cx.run_until_parked();
        assert_eq!(notified.get(), 1, "an unchanged tree must not repaint");
        monitor.read_with(cx, |monitor, _| {
            assert!(Arc::ptr_eq(monitor.snapshot.as_ref().unwrap(), &before));
        });
    }

    #[gpui::test]
    fn a_refresh_during_a_read_runs_once_that_read_lands(cx: &mut gpui::TestAppContext) {
        let root = repo();
        let data = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| AppState::restore(fx_store::AppPaths::at(data.path()), cx));
        state.update(cx, |state, _| {
            state.add_project(&root.path().to_string_lossy());
        });
        let monitor = cx.new(|_| RepositoryMonitor::new(state));
        monitor.update(cx, |monitor, cx| {
            monitor.sync(cx);
            assert_eq!(monitor.reads, 1);
            monitor.refresh(cx);
            assert_eq!(monitor.reads, 1, "a second read never overlaps the first");
            assert!(monitor.queued);
            let generation = monitor.generation;
            monitor.complete_read(generation, Ok(snapshot(root.path())), cx);
            assert_eq!(monitor.reads, 2, "the queued read follows immediately");
            assert!(!monitor.queued);
        });
    }
}
