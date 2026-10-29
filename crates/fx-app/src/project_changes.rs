//! Live project Git summary in the title bar, independent of inspector visibility.
//!
//! It shares the inspector's [`RepositoryMonitor`] rather than polling on its
//! own, so the title bar and the inspector can never disagree about the tree.

use gpui::prelude::*;
use gpui::{AnyElement, AnyView, App, Context, Entity, Subscription, Window, div};

use crate::inspector::monitor::{self, RepositoryMonitor};
use crate::state::AppState;

#[derive(Clone)]
struct ChangesHandle(Entity<ProjectChanges>);
impl gpui::Global for ChangesHandle {}

struct ProjectChanges {
    state: Entity<AppState>,
    monitor: Entity<RepositoryMonitor>,
    inspector_visible: bool,
    _observation: Subscription,
}

pub(crate) fn view(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let existing = cx
        .try_global::<ChangesHandle>()
        .map(|handle| handle.0.clone())
        .filter(|changes| changes.read(cx).state == state);
    let changes = existing.unwrap_or_else(|| {
        let monitor = monitor::shared(&state, cx);
        let changes = cx.new(|cx| ProjectChanges::new(state, monitor, cx));
        cx.set_global(ChangesHandle(changes.clone()));
        changes
    });
    changes.update(cx, |changes, cx| changes.sync(cx));
    AnyView::from(changes).into_any_element()
}

impl ProjectChanges {
    fn new(
        state: Entity<AppState>,
        monitor: Entity<RepositoryMonitor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&monitor, |_, _, cx| cx.notify());
        Self {
            state,
            monitor,
            inspector_visible: false,
            _observation: observation,
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.monitor.update(cx, |monitor, cx| monitor.sync(cx));
        let inspector_visible = self.state.read(cx).snapshot.right_panel_visible;
        if self.inspector_visible != inspector_visible {
            self.inspector_visible = inspector_visible;
            cx.notify();
        }
    }
}

impl Render for ProjectChanges {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let monitor = self.monitor.read(cx);
        // Never present stale line totals as current after a failed read. The
        // inspector remains accessible through its permanent toolbar control.
        if monitor.error().is_some() {
            return div().into_any_element();
        }
        let Some(snapshot) = monitor.snapshot() else {
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
            "{} changed {} · Current chat workspace\nClick to toggle Changes · Includes staged, unstaged and new files",
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
        let (additions, deletions, files) = (repo.additions, repo.deletions, repo.files.len());
        let state = self.state.clone();
        fx_design::components::change_indicator(
            if partial { 0 } else { additions },
            if partial { 0 } else { deletions },
            files,
            state.read(cx).snapshot.right_panel_visible,
            detail,
            move |_, _, cx| {
                state.update(cx, |state, cx| {
                    state.toggle_inspector();
                    state.save_after_panel_moves(cx);
                    cx.notify();
                });
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::repository::{self, Snapshot};
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

    /// A title bar whose monitor holds `snapshot`.
    fn changes_with(
        test: &mut gpui::TestAppContext,
        state: &Entity<AppState>,
        snapshot: Snapshot,
    ) -> Entity<ProjectChanges> {
        test.update(|cx| {
            let monitor = cx.new(|_| RepositoryMonitor::new(state.clone()));
            monitor.update(cx, |monitor, cx| monitor.complete_read(0, Ok(snapshot), cx));
            cx.new(|cx| ProjectChanges::new(state.clone(), monitor, cx))
        })
    }

    struct TestView(Entity<ProjectChanges>);
    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w_full()
                .h_full()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, _| {
                    panic!("a change-indicator click must not reach the title-bar drag handler")
                })
                .child(fx_design::components::titlebar_control(self.0.clone()))
        }
    }

    #[test]
    fn title_indicator_toggles_changes_and_disappears_when_clean() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "edited\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        let changes = changes_with(&mut test, &state, snapshot(root.path()));
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
        let monitor = test.read(|cx| changes.read(cx).monitor.clone());
        monitor.update(&mut test, |monitor, cx| {
            monitor.complete_read(0, Ok(snapshot(root.path())), cx)
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
    fn a_failed_read_hides_stale_totals() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "edited\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        let changes = changes_with(&mut test, &state, snapshot(root.path()));
        let monitor = test.read(|cx| changes.read(cx).monitor.clone());
        monitor.update(&mut test, |monitor, cx| {
            monitor.complete_read(0, Err("read failed".into()), cx)
        });
        let window = test.add_window(move |_, _| TestView(changes));
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        assert!(visual.debug_bounds("project-change-indicator").is_none());
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
        let changes = changes_with(&mut test, &state, snapshot);
        let window = test.add_window(move |_, _| TestView(changes));
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        assert!(visual.debug_bounds("project-change-indicator").is_some());
        assert!(visual.debug_bounds("project-added-count").is_none());
        assert!(visual.debug_bounds("project-deleted-count").is_none());
    }

    #[test]
    fn the_title_bar_and_the_inspector_share_one_poller() {
        let root = repo();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = state(&mut test, data.path());
        state.update(&mut test, |state, _| {
            state.add_project(&root.path().to_string_lossy());
            state.toggle_inspector();
        });
        test.update(|cx| {
            assert!(state.read(cx).snapshot.right_panel_visible);
            let _ = view(state.clone(), cx);
            let _ = crate::inspector::view(state.clone(), cx);
            // Both again, as every frame does.
            let _ = view(state.clone(), cx);
            let _ = crate::inspector::view(state.clone(), cx);
            let monitor = monitor::shared(&state, cx);
            let title = cx.global::<ChangesHandle>().0.read(cx).monitor.clone();
            assert_eq!(title, monitor, "the title bar must use the shared monitor");
            assert_eq!(
                monitor.read(cx).reads(),
                1,
                "opening a project reads it once, not once per view"
            );
        });
    }
}
