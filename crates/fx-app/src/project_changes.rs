//! Live project Git summary in the title bar, independent of inspector visibility.

use std::path::PathBuf;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{AnyElement, AnyView, App, Context, Entity, Task, Window, div};

use crate::inspector::repository::{self, Snapshot};
use crate::state::AppState;

#[derive(Clone)]
struct ChangesHandle(Entity<ProjectChanges>);
impl gpui::Global for ChangesHandle {}

struct ProjectChanges {
    state: Entity<AppState>,
    source: Option<String>,
    generation: u64,
    was_busy: bool,
    inspector_visible: bool,
    snapshot: Option<Snapshot>,
    loading: bool,
    read_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
}

pub(crate) fn view(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let existing = cx
        .try_global::<ChangesHandle>()
        .map(|handle| handle.0.clone())
        .filter(|changes| changes.read(cx).state == state);
    let changes = existing.unwrap_or_else(|| {
        let changes = cx.new(|_| ProjectChanges::new(state));
        cx.set_global(ChangesHandle(changes.clone()));
        changes
    });
    changes.update(cx, |changes, cx| changes.sync(cx));
    AnyView::from(changes).into_any_element()
}

impl ProjectChanges {
    fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            source: None,
            generation: 0,
            was_busy: false,
            inspector_visible: false,
            snapshot: None,
            loading: false,
            read_task: None,
            poll_task: None,
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let source = state
            .active_project()
            .map(|project| project.root_path.clone());
        let busy = state.is_busy();
        let inspector_visible = state.snapshot.right_panel_visible;
        if self.inspector_visible != inspector_visible {
            self.inspector_visible = inspector_visible;
            cx.notify();
        }
        if self.source != source {
            self.source = source;
            self.generation += 1;
            self.read_task = None;
            self.loading = false;
            self.snapshot = None;
            self.refresh(cx);
            cx.notify();
        } else if self.was_busy && !busy {
            self.refresh(cx);
        }
        self.was_busy = busy;
        if self.poll_task.is_none() {
            self.poll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(Duration::from_secs(5)).await;
                    if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.source.clone() else {
            return;
        };
        if self.loading {
            return;
        }
        self.loading = true;
        let generation = self.generation;
        let task = repository::spawn_read(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(repository::changes_snapshot(&PathBuf::from(root)))
        });
        self.read_task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_read(task).await;
            let _ = this.update(cx, |this, cx| this.complete_read(generation, result, cx));
        }));
    }

    fn complete_read(
        &mut self,
        generation: u64,
        result: Result<Snapshot, String>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        self.loading = false;
        // Never present stale line totals as current after a failed read. The
        // inspector remains accessible through its permanent toolbar control.
        self.snapshot = result.ok();
        cx.notify();
    }
}

impl Render for ProjectChanges {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = &self.snapshot else {
            return div().into_any_element();
        };
        let Some(repo) = snapshot
            .repository
            .as_ref()
            .filter(|repo| !repo.files.is_empty())
        else {
            return div().into_any_element();
        };
        let partial = snapshot.notice.is_some();
        let mut detail = format!(
            "{} changed {} · Live project working tree\nClick to toggle Changes · Includes staged, unstaged and new files",
            repo.files.len(),
            if repo.files.len() == 1 {
                "file"
            } else {
                "files"
            }
        );
        if let Some(notice) = &snapshot.notice {
            detail.push('\n');
            detail.push_str(notice);
        }
        let state = self.state.clone();
        fx_design::components::change_indicator(
            if partial { 0 } else { repo.additions },
            if partial { 0 } else { repo.deletions },
            repo.files.len(),
            state.read(cx).snapshot.right_panel_visible,
            detail,
            move |_, _, cx| {
                state.update(cx, |state, cx| {
                    state.toggle_inspector();
                    state.save();
                    cx.notify();
                });
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;
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

    fn snapshot(root: &std::path::Path) -> Snapshot {
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

    struct TestView(Entity<ProjectChanges>);
    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w_full().h_full().child(self.0.clone())
        }
    }

    #[test]
    fn title_indicator_toggles_changes_and_disappears_when_clean() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "edited\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        let changes = test.update(|cx| {
            cx.new(|_| {
                let mut changes = ProjectChanges::new(state.clone());
                changes.snapshot = Some(snapshot(root.path()));
                changes
            })
        });
        let mounted = changes.clone();
        let window = test.add_window(move |_, _| TestView(mounted));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(gpui::size(px(500.0), px(100.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let bounds = visual.debug_bounds("project-change-indicator").unwrap();
        assert!(bounds.size.height <= px(32.0));
        for expected in [true, false] {
            visual.simulate_mouse_down(
                bounds.center(),
                gpui::MouseButton::Left,
                Default::default(),
            );
            visual.simulate_mouse_up(bounds.center(), gpui::MouseButton::Left, Default::default());
            test.read(|cx| assert_eq!(state.read(cx).snapshot.right_panel_visible, expected));
            test.refresh().unwrap();
            test.update(|_| {});
        }
        std::fs::write(root.path().join("sample.txt"), "original\n").unwrap();
        changes.update(&mut test, |changes, cx| {
            changes.complete_read(0, Ok(snapshot(root.path())), cx)
        });
        test.refresh().unwrap();
        test.update(|_| {});
        // GPUI's debug-bound registry retains removed selectors on a window.
        // Mount the clean state in a fresh window to inspect its actual tree.
        let clean_window = test.add_window(move |_, _| TestView(changes));
        test.refresh().unwrap();
        let mut clean = gpui::VisualTestContext::from_window(clean_window.into(), &test);
        assert!(clean.debug_bounds("project-change-indicator").is_none());
    }

    #[test]
    fn switched_project_rejects_previous_counts_and_failed_reads_drop_stale_totals() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "edited\n").unwrap();
        let other = repo();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        state.update(&mut test, |state, _| {
            state.add_project(&root.path().to_string_lossy());
        });
        let changes = test.update(|cx| cx.new(|_| ProjectChanges::new(state.clone())));
        let generation = changes.update(&mut test, |changes, cx| {
            changes.sync(cx);
            changes.complete_read(changes.generation, Ok(snapshot(root.path())), cx);
            assert!(changes.snapshot.is_some());
            changes.generation
        });
        state.update(&mut test, |state, _| {
            state.add_project(&other.path().to_string_lossy());
        });
        changes.update(&mut test, |changes, cx| {
            changes.sync(cx);
            assert!(changes.snapshot.is_none());
            changes.complete_read(generation, Ok(snapshot(root.path())), cx);
            assert!(
                changes.snapshot.is_none(),
                "the previous project's read must be ignored"
            );
            changes.complete_read(changes.generation, Ok(snapshot(other.path())), cx);
            assert!(
                changes
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .repository
                    .as_ref()
                    .unwrap()
                    .files
                    .is_empty()
            );
            changes.complete_read(changes.generation, Err("read failed".into()), cx);
            assert!(changes.snapshot.is_none());
        });
    }

    #[test]
    fn binary_or_partial_changes_have_a_file_count_instead_of_false_line_totals() {
        let root = repo();
        std::fs::write(root.path().join("binary.dat"), b"a\0b").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        let mut snapshot = snapshot(root.path());
        snapshot.notice = Some("Line counts are incomplete".into());
        let changes = test.update(|cx| {
            cx.new(|_| {
                let mut changes = ProjectChanges::new(state);
                changes.snapshot = Some(snapshot);
                changes
            })
        });
        let window = test.add_window(move |_, _| TestView(changes));
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        assert!(visual.debug_bounds("project-change-indicator").is_some());
        assert!(visual.debug_bounds("project-added-count").is_none());
        assert!(visual.debug_bounds("project-deleted-count").is_none());
    }
}
