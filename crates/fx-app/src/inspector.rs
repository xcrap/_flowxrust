//! Live changes inspector: the working tree's diff, and committing and pushing it.
//!
//! The inspector reads the shared [`RepositoryMonitor`] snapshot rather than
//! polling for itself, shows the project diff in one of three comparisons
//! (unstaged, staged, or against `HEAD`), and writes to the repository only
//! when asked: a commit from the composer, or a push from the header.

#[path = "git_inspector.rs"]
pub(crate) mod repository;

#[path = "repository_monitor.rs"]
pub(crate) mod monitor;

#[path = "commit_composer.rs"]
mod commit_composer;

#[path = "diff_view.rs"]
mod diff_view;

use fx_core::GitRepositoryInfo;
use fx_design::components::{
    DividerAxis, IconButtonProps, compact_action_toggle, divider_with_axis, icon_button,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, IconSize, Layout, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{
    AnyElement, AnyView, App, Context, Entity, SharedString, Subscription, Window, div, px,
};

use crate::state::AppState;
use commit_composer::{CommitComposer, Operation};
use diff_view::DiffState;
use monitor::RepositoryMonitor;

const INSPECTOR_HEADER_HEIGHT: f32 = Layout::PANE_HEADER_HEIGHT;

#[derive(Clone)]
struct InspectorHandle(Entity<Inspector>);
impl gpui::Global for InspectorHandle {}

struct Inspector {
    state: Entity<AppState>,
    monitor: Entity<RepositoryMonitor>,
    /// Visible. A hidden inspector skips diff loads; the monitor keeps
    /// polling for the title bar regardless.
    active: bool,
    project: Option<String>,
    diff: DiffState,
    composer: CommitComposer,
    /// Why opening a file failed.
    notice: Option<String>,
    _observation: Subscription,
}

/// The child view owns asynchronous reads and receives GPUI notifications
/// directly, so results repaint even when no conversation is busy.
pub fn view(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let visible = state.read(cx).snapshot.right_panel_visible;
    if !visible {
        if let Some(handle) = cx.try_global::<InspectorHandle>().cloned() {
            handle.0.update(cx, |inspector, _| inspector.active = false);
        }
        return div().w(px(0.0)).h_full().flex_shrink_0().into_any_element();
    }
    let existing = cx
        .try_global::<InspectorHandle>()
        .map(|handle| handle.0.clone())
        .filter(|inspector| inspector.read(cx).state == state);
    let inspector = existing.unwrap_or_else(|| {
        let monitor = monitor::shared(&state, cx);
        let inspector = cx.new(|cx| Inspector::new(state.clone(), monitor, cx));
        cx.set_global(InspectorHandle(inspector.clone()));
        inspector
    });
    inspector.update(cx, |inspector, cx| inspector.sync(cx));
    AnyView::from(inspector).into_any_element()
}

impl Inspector {
    fn new(
        state: Entity<AppState>,
        monitor: Entity<RepositoryMonitor>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observation = cx.observe(&monitor, |this: &mut Self, _, cx| {
            if this.active {
                this.ensure_diff(cx);
            }
            cx.notify();
        });
        Self {
            state,
            monitor,
            active: true,
            project: None,
            diff: DiffState::default(),
            composer: CommitComposer::new(cx),
            notice: None,
            _observation: observation,
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.monitor.update(cx, |monitor, cx| monitor.sync(cx));
        let project = self.monitor.read(cx).project().map(str::to_string);
        if self.project != project {
            self.composer
                .switch_project(self.project.as_deref(), project.as_deref(), cx);
            self.project = project;
            self.diff.reset();
            self.notice = None;
            cx.notify();
        }
        self.ensure_diff(cx);
    }

    /// The repository as last read, if the project is one.
    fn repository<'a>(&self, cx: &'a App) -> Option<&'a GitRepositoryInfo> {
        self.monitor.read(cx).snapshot()?.repository.as_ref()
    }

    fn header(&self, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let repository = self.repository(cx);
        let running = self.composer.running;
        let mut header = div()
            .h(px(INSPECTOR_HEADER_HEIGHT))
            .debug_selector(|| "inspector-header".into())
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px_len(Spacing::XXS))
            .px(px_len(Spacing::MD))
            .child(
                div()
                    .debug_selector(|| "inspector-heading".into())
                    .fx_body_medium()
                    .text_color(Colors::fg(&theme))
                    .child("Changes"),
            )
            .child(div().flex_1());
        if repository.is_some_and(GitRepositoryInfo::has_changes) || self.composer.visible {
            header = header.child(compact_action_toggle(
                if self.composer.visible {
                    "Cancel"
                } else {
                    "Commit"
                },
                Some(if self.composer.visible {
                    Icon::XMARK
                } else {
                    Icon::CHECKMARK
                }),
                self.composer.visible,
                running.is_none(),
                cx.listener(|this, _, window, cx| this.toggle_composer(window, cx)),
            ));
        }
        if repository.is_some_and(GitRepositoryInfo::can_push) || running == Some(Operation::Push) {
            header = header.child(compact_action_toggle(
                if running == Some(Operation::Push) {
                    "Pushing…"
                } else {
                    "Push"
                },
                Some(Icon::ARROW_UP),
                false,
                running.is_none(),
                cx.listener(|this, _, _, cx| this.push(cx)),
            ));
        }
        header
            .child(icon_button(
                IconButtonProps::new(Icon::ARROW_CLOCKWISE, "Refresh changes")
                    .small()
                    .ghost()
                    .enabled(self.project.is_some() && !self.monitor.read(cx).is_loading()),
                cx.listener(|this, _, _, cx| this.refresh(cx)),
            ))
            .into_any_element()
    }

    fn summary(&self, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let monitor = self.monitor.read(cx);
        let repository = self.repository(cx);
        let heading = repository
            .map(|repo| repo.branch.clone())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| {
                self.state
                    .read(cx)
                    .active_project()
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| "No project".into())
            });
        let tracking = repository
            .map(|repo| {
                let mut parts = Vec::new();
                if repo.ahead_count > 0 {
                    parts.push(format!("↑{}", repo.ahead_count));
                }
                if repo.behind_count > 0 {
                    parts.push(format!("↓{}", repo.behind_count));
                }
                if repo.upstream_gone {
                    parts.push("upstream gone".into());
                }
                parts.join(" ")
            })
            .unwrap_or_default();
        let count = match (monitor.snapshot(), repository) {
            (Some(_), Some(repo)) => format!("{} changed", repo.files.len()),
            (Some(_), None) => String::new(),
            (None, _) if monitor.is_loading() => "Reading…".into(),
            (None, _) => String::new(),
        };
        div()
            .h(px(Layout::CONTROL_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px_len(Spacing::SM))
            .px(px_len(Spacing::MD))
            .child(
                if repository.is_some() {
                    Icon::ARROW_TRIANGLE_BRANCH
                } else {
                    Icon::FOLDER
                }
                .render(IconSize::Small, Colors::fg_tertiary(&theme), &theme),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(SharedString::from(heading)),
            )
            .children((!tracking.is_empty()).then(|| {
                div()
                    .debug_selector(|| "inspector-tracking".into())
                    .flex_shrink_0()
                    .fx_role(TextRole::MonoSmall)
                    .text_color(Colors::fg_tertiary(&theme))
                    .child(SharedString::from(tracking))
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .fx_role(TextRole::Caption)
                    .text_color(Colors::fg_tertiary(&theme))
                    .child(SharedString::from(count)),
            )
            .into_any_element()
    }

    fn body(&self, cx: &Context<Self>) -> AnyElement {
        let monitor = self.monitor.read(cx);
        let Some(snapshot) = monitor.snapshot() else {
            return note(
                if self.project.is_none() {
                    "Select a project to view its changes."
                } else if monitor.is_loading() {
                    "Reading project…"
                } else {
                    "Use Refresh to read this project."
                },
                false,
            );
        };
        let Some(repository) = &snapshot.repository else {
            return note("This folder has no Git repository to track changes.", false);
        };
        if repository.files.is_empty() {
            return inspector_empty(
                Icon::CHECKMARK_CIRCLE_FILL,
                "Working tree clean",
                "File changes will appear here as you work.",
            );
        }
        self.diff_body(repository, cx)
    }
}

impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let mut view = div()
            .w_full()
            .debug_selector(|| "inspector-root".into())
            .min_w_0()
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(Colors::bg_elevated(&theme))
            .border_l(px_len(BorderWidth::HAIRLINE))
            .border_color(Colors::border_subtle(&theme))
            .child(self.header(cx))
            .child(divider_with_axis(DividerAxis::Horizontal))
            .child(self.summary(cx))
            .children(self.operation_status(cx));
        if self.composer.visible {
            view = view
                .child(self.composer_view(cx))
                .child(divider_with_axis(DividerAxis::Horizontal));
        }
        let monitor = self.monitor.read(cx);
        if let Some(error) = monitor.error() {
            view = view.child(note(error, true));
        }
        if let Some(notice) = monitor.snapshot().and_then(|s| s.notice.as_ref()) {
            view = view.child(note(notice, false));
        }
        if let Some(notice) = &self.notice {
            view = view.child(note(notice, true));
        }
        view.child(self.body(cx))
    }
}

fn split_file_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit_once('/')
        .map_or((path, ""), |(parent, name)| (name, parent))
}

fn inspector_empty(icon: Icon, title: &str, hint: &str) -> AnyElement {
    let theme = global::current();
    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .justify_center()
        .p(px_len(Spacing::MD))
        .child(fx_design::components::empty_state(
            icon.render(IconSize::Illustration, Colors::fg_tertiary(&theme), &theme),
            SharedString::from(title.to_string()),
            SharedString::from(hint.to_string()),
        ))
        .into_any_element()
}

fn note(text: &str, error: bool) -> AnyElement {
    let theme = global::current();
    div()
        .flex_shrink_0()
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .fx_caption()
        .text_color(if error {
            Colors::error(&theme)
        } else {
            Colors::fg_secondary(&theme)
        })
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use commit_composer::{CommitChoice, Outcome};
    use diff_view::{DiffKey, DiffLayout};
    use fx_core::git::DiffLineKind;
    use repository::{ComparisonMode, DiffDocument, Snapshot};

    struct NarrowInspector {
        inner: Entity<Inspector>,
        width: f32,
    }

    impl Render for NarrowInspector {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).h_full().child(self.inner.clone())
        }
    }

    fn git(root: &std::path::Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        git(
            temp.path(),
            &["config", "user.email", "flowx-test@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "FlowX test"]);
        git(temp.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(temp.path().join("sample.txt"), "before the edit\n").unwrap();
        std::fs::write(temp.path().join("unchanged.txt"), "leave this file alone\n").unwrap();
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "Initial fixture"]);
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

    /// An inspector mounted at `width`. With a project, it is opened the way
    /// the shell opens one and its first read and diff are awaited.
    fn mount(
        test: &mut gpui::TestAppContext,
        data: &std::path::Path,
        project: Option<&std::path::Path>,
        width: f32,
    ) -> (Entity<Inspector>, gpui::WindowHandle<NarrowInspector>) {
        let state =
            test.update(|cx| cx.new(|cx| AppState::restore(fx_store::AppPaths::at(data), cx)));
        if let Some(project) = project {
            state.update(test, |state, _| {
                state.add_project(&project.to_string_lossy());
            });
        }
        let inspector = test.update(|cx| {
            let monitor = cx.new(|_| RepositoryMonitor::new(state.clone()));
            let inspector = cx.new(|cx| Inspector::new(state.clone(), monitor, cx));
            inspector.update(cx, |inspector, cx| inspector.sync(cx));
            inspector
        });
        if project.is_some() {
            wait_until(test, &inspector, |i| {
                i.diff.loading.is_none() && i.diff.shown.is_some()
            });
        }
        let inner = inspector.clone();
        let window = test.add_window(move |_, _| NarrowInspector { inner, width });
        (inspector, window)
    }

    /// Hand the monitor a read, as a poll landing would.
    fn deliver(test: &mut gpui::TestAppContext, inspector: &Entity<Inspector>, snapshot: Snapshot) {
        let monitor = test.read(|cx| inspector.read(cx).monitor.clone());
        monitor.update(test, |monitor, cx| {
            let generation = monitor.generation();
            monitor.complete_read(generation, Ok(snapshot), cx)
        });
    }

    /// The Git worker is a real thread outside GPUI's fake dispatcher, so
    /// delivery is pumped while waiting.
    fn wait_until(
        test: &mut gpui::TestAppContext,
        inspector: &Entity<Inspector>,
        done: impl Fn(&Inspector) -> bool,
    ) {
        for _ in 0..400 {
            test.run_until_parked();
            if test.read(|cx| done(inspector.read(cx))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for the inspector");
    }

    #[test]
    fn inspector_controls_fit_the_compact_panel_without_overlap() {
        for width in [280.0, 360.0] {
            let temp = tempfile::tempdir().unwrap();
            let mut test = gpui::TestAppContext::single();
            let (_, window) = mount(&mut test, temp.path(), None, width);
            test.refresh().unwrap();
            let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
            let root = visual.debug_bounds("inspector-root").unwrap();
            let heading = visual.debug_bounds("inspector-heading").unwrap();
            assert!(visual.debug_bounds("segmented-control").is_none());
            let refresh = visual.debug_bounds("icon-button:Refresh changes").unwrap();
            assert!(visual.debug_bounds("icon-button:Hide inspector").is_none());
            assert_eq!(root.size.width, px(width));
            assert!(heading.left() > root.left());
            assert!(heading.right() < refresh.left());
            assert_eq!(root.right() - refresh.right(), px(Spacing::MD));
            assert_eq!(heading.center().y, refresh.center().y);
        }
    }

    #[test]
    fn selecting_a_changed_file_paints_its_diff_without_showing_unchanged_files() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "after the edit\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        wait_until(&mut test, &inspector, |i| i.diff.document.is_some());
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        let position = visual.debug_bounds("inspector-file-row").unwrap().center();
        visual.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(position, gpui::MouseButton::Left, Default::default());
        test.read(|cx| {
            let inspector = inspector.read(cx);
            assert_eq!(inspector.diff.selected_path.as_deref(), Some("sample.txt"));
            let document = inspector.diff.document.as_ref().unwrap();
            assert_eq!(
                document.sections.len(),
                1,
                "unchanged files have no section"
            );
            let lines: Vec<_> = document.sections[0]
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect();
            assert!(lines.contains(&"-before the edit"));
            assert!(lines.contains(&"+after the edit"));
        });
        test.refresh().unwrap();
        assert!(visual.debug_bounds("diff-header").is_some());
        assert!(visual.debug_bounds("diff-line").is_some());
    }

    #[test]
    fn a_refresh_keeps_the_open_diff_its_selection_and_its_scroll_position() {
        let root = repo();
        let body = |n: usize| -> String { (0..n).map(|i| format!("line {i}\n")).collect() };
        std::fs::write(root.path().join("sample.txt"), body(200)).unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        wait_until(&mut test, &inspector, |i| i.diff.document.is_some());
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(gpui::size(px(360.0), px(500.0)));
        test.refresh().unwrap();
        let height = diff_view::row_height(&global::current());
        let scrolled = -(height * 40.0);
        inspector.update(&mut test, |inspector, cx| {
            inspector.select_path("sample.txt", cx)
        });
        // Let the selection's scroll-to-header land before scrolling away.
        test.refresh().unwrap();
        let first = inspector.update(&mut test, |inspector, _| {
            let handle = inspector.diff.scroll.0.borrow().base_handle.clone();
            handle.set_offset(gpui::point(px(0.0), px(scrolled)));
            inspector.diff.document.clone().unwrap()
        });
        test.refresh().unwrap();

        // The agent keeps writing: new line counts, a new revision.
        std::fs::write(root.path().join("sample.txt"), body(201)).unwrap();
        deliver(&mut test, &inspector, snapshot(root.path()));
        test.read(|cx| {
            let inspector = inspector.read(cx);
            assert!(inspector.diff.loading.is_some(), "a new revision reloads");
            assert!(
                Arc::ptr_eq(inspector.diff.document.as_ref().unwrap(), &first),
                "the open diff must stay while its replacement loads"
            );
            assert_eq!(inspector.diff.selected_path.as_deref(), Some("sample.txt"));
        });
        test.refresh().unwrap();
        assert!(
            visual.debug_bounds("diff-canvas").is_some(),
            "no loading placeholder"
        );
        let offset = |test: &gpui::TestAppContext| {
            test.read(|cx| {
                f32::from(
                    inspector
                        .read(cx)
                        .diff
                        .scroll
                        .0
                        .borrow()
                        .base_handle
                        .offset()
                        .y,
                )
            })
        };
        assert_eq!(offset(&test), scrolled);

        wait_until(&mut test, &inspector, |i| i.diff.loading.is_none());
        test.read(|cx| {
            let inspector = inspector.read(cx);
            let document = inspector.diff.document.as_ref().unwrap();
            assert!(!Arc::ptr_eq(document, &first), "the new content arrived");
            assert_eq!(document.sections[0].additions, 201);
            assert_eq!(inspector.diff.selected_path.as_deref(), Some("sample.txt"));
        });
        assert_eq!(offset(&test), scrolled, "the reader stays on the same line");
        test.refresh().unwrap();
        assert_eq!(offset(&test), scrolled);
    }

    #[test]
    fn an_unchanged_poll_neither_reloads_nor_repaints_the_diff() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "after the edit\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, _window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        let first = test.read(|cx| inspector.read(cx).diff.document.clone().unwrap());
        deliver(&mut test, &inspector, snapshot(root.path()));
        test.run_until_parked();
        test.read(|cx| {
            let inspector = inspector.read(cx);
            assert!(inspector.diff.loading.is_none());
            assert!(Arc::ptr_eq(
                inspector.diff.document.as_ref().unwrap(),
                &first
            ));
        });
    }

    #[test]
    fn modes_layouts_and_disclosure_change_what_the_canvas_draws() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "staged\n").unwrap();
        git(root.path(), &["add", "sample.txt"]);
        std::fs::write(root.path().join("sample.txt"), "staged and then edited\n").unwrap();
        std::fs::write(root.path().join("new.txt"), "brand new\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        wait_until(&mut test, &inspector, |i| i.diff.document.is_some());
        let paths = |test: &gpui::TestAppContext| -> Vec<String> {
            test.read(|cx| {
                inspector
                    .read(cx)
                    .diff
                    .document
                    .as_ref()
                    .unwrap()
                    .sections
                    .iter()
                    .map(|s| s.path.clone())
                    .collect()
            })
        };
        assert_eq!(paths(&test), ["new.txt", "sample.txt"]);
        inspector.update(&mut test, |inspector, cx| {
            inspector.set_mode(ComparisonMode::Staged, cx)
        });
        test.read(|cx| {
            assert!(
                inspector.read(cx).diff.document.is_none(),
                "no stale comparison"
            )
        });
        wait_until(&mut test, &inspector, |i| i.diff.document.is_some());
        assert_eq!(paths(&test), ["sample.txt"]);
        let staged: Vec<String> = test.read(|cx| {
            inspector.read(cx).diff.document.as_ref().unwrap().sections[0]
                .lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Addition)
                .map(|line| line.text.clone())
                .collect()
        });
        assert_eq!(staged, ["+staged"]);

        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        inspector.update(&mut test, |inspector, cx| {
            inspector.set_layout(DiffLayout::Split, cx)
        });
        test.refresh().unwrap();
        assert!(visual.debug_bounds("diff-split-row").is_some());

        let rows = |test: &gpui::TestAppContext| {
            test.read(|cx| {
                let inspector = inspector.read(cx);
                inspector
                    .diff
                    .row_layout(inspector.diff.document.as_ref().unwrap())
                    .count()
            })
        };
        let expanded = rows(&test);
        let mut click = |name: &'static str| {
            let at = visual
                .debug_bounds(name)
                .unwrap_or_else(|| panic!("{name} is not drawn"))
                .center();
            visual.simulate_mouse_down(at, gpui::MouseButton::Left, Default::default());
            visual.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
        };
        click("icon-button:Collapse all diffs");
        assert_eq!(rows(&test), 1, "one header per file");
        test.refresh().unwrap();
        // The same control now offers the other direction.
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        let at = visual
            .debug_bounds("icon-button:Expand all diffs")
            .unwrap()
            .center();
        visual.simulate_mouse_down(at, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
        assert_eq!(rows(&test), expanded);
    }

    #[test]
    fn every_header_and_toolbar_control_fits_the_narrowest_panel() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "after the edit\n").unwrap();
        // Something staged, so the composer offers all three scopes.
        git(root.path(), &["add", "sample.txt"]);
        // A remote with nothing pushed yet, so Push is offered.
        git(
            root.path(),
            &["remote", "add", "origin", "/nonexistent/remote.git"],
        );
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let width = Layout::RIGHT_PANEL_MIN_WIDTH;
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), width);
        inspector.update(&mut test, |inspector, cx| {
            inspector.set_mode(ComparisonMode::Base, cx)
        });
        wait_until(&mut test, &inspector, |i| i.diff.document.is_some());
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        window
            .update(&mut test, |root, window, cx| {
                root.inner
                    .update(cx, |inspector, cx| inspector.toggle_composer(window, cx));
            })
            .unwrap();
        test.refresh().unwrap();
        let mut bounds = |name: &'static str| {
            visual
                .debug_bounds(name)
                .unwrap_or_else(|| panic!("{name} is not drawn"))
        };
        let panel = bounds("inspector-root");
        let inner_right = panel.right() - px(Spacing::MD);
        let header: &[&'static str] = &[
            "inspector-heading",
            "compact-action:Cancel",
            "compact-action:Push",
            "icon-button:Refresh changes",
        ];
        let toolbar: &[&'static str] = &[
            "compact-action:Files 1",
            "icon-button:Collapse all diffs",
            "diff-layout-picker",
        ];
        for row in [header, toolbar] {
            let placed: Vec<_> = row.iter().map(|name| (*name, bounds(name))).collect();
            for pair in placed.windows(2) {
                let ((left_name, left), (right_name, right)) = (pair[0], pair[1]);
                assert!(
                    left.right() <= right.left(),
                    "{left_name} overlaps {right_name}: {left:?} {right:?}"
                );
            }
            let (last, last_bounds) = placed.last().unwrap();
            assert!(
                last_bounds.right() <= inner_right + px(0.5),
                "{last} runs past the panel edge: {placed:?} in {panel:?}"
            );
        }
        for name in ["diff-mode-picker", "commit-scope-picker", "commit-button"] {
            let placed = bounds(name);
            assert!(
                placed.right() <= inner_right + px(0.5),
                "{name} runs past the panel edge: {placed:?}"
            );
        }
    }

    #[test]
    fn a_failed_diff_is_not_retried_on_every_frame_but_refresh_retries_it() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "after the edit\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, _) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        inspector.update(&mut test, |inspector, cx| {
            let key = inspector.diff.shown.take().unwrap();
            inspector.diff.loading = Some(key);
            inspector.finish_diff(key, Err("diff failed".into()), cx);
            assert_eq!(inspector.diff.error.as_deref(), Some("diff failed"));
            // What every render does.
            inspector.ensure_diff(cx);
            inspector.ensure_diff(cx);
            assert!(inspector.diff.loading.is_none(), "a failure must not loop");
            inspector.refresh(cx);
            assert!(inspector.diff.loading.is_some(), "Refresh retries the diff");
        });
    }

    #[test]
    fn late_diffs_cannot_cross_a_project_or_comparison_switch() {
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, _) = mount(&mut test, data.path(), None, 360.0);
        inspector.update(&mut test, |inspector, cx| {
            let stale = DiffKey {
                generation: 0,
                mode: ComparisonMode::Unstaged,
                revision: 7,
            };
            inspector.diff.loading = Some(stale);
            inspector.diff.mode = ComparisonMode::Staged;
            inspector.diff.loading = Some(DiffKey {
                mode: ComparisonMode::Staged,
                ..stale
            });
            inspector.finish_diff(stale, Ok(DiffDocument::default()), cx);
            assert!(
                inspector.diff.document.is_none(),
                "a superseded load is dropped"
            );
            inspector.diff.reset();
            inspector.finish_diff(stale, Ok(DiffDocument::default()), cx);
            assert!(inspector.diff.document.is_none());
        });
    }

    #[test]
    fn committing_from_the_composer_commits_clears_the_draft_and_refreshes() {
        let root = repo();
        std::fs::write(root.path().join("sample.txt"), "after the edit\n").unwrap();
        std::fs::write(root.path().join("new.txt"), "new\n").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        window
            .update(&mut test, |root, window, cx| {
                root.inner
                    .update(cx, |inspector, cx| inspector.toggle_composer(window, cx));
            })
            .unwrap();
        test.refresh().unwrap();
        assert!(visual.debug_bounds("commit-composer").is_some());
        inspector.update(&mut test, |inspector, cx| {
            assert_eq!(inspector.composer.choice, CommitChoice::All);
            inspector.commit("   ".into(), cx);
            assert!(matches!(
                inspector.composer.outcome,
                Some(Outcome::Failed(..))
            ));
            inspector
                .composer
                .field
                .update(cx, |field, cx| field.set_value("Ship it", cx));
            inspector.commit("Ship it".into(), cx);
            assert_eq!(inspector.composer.running, Some(Operation::Commit));
        });
        test.refresh().unwrap();
        assert!(visual.debug_bounds("git-operation-progress").is_some());
        let reads = test.read(|cx| inspector.read(cx).monitor.read(cx).reads());
        wait_until(&mut test, &inspector, |i| i.composer.running.is_none());
        test.read(|cx| {
            let inspector = inspector.read(cx);
            assert!(
                matches!(&inspector.composer.outcome, Some(Outcome::Succeeded(summary)) if summary.contains("Ship it")),
                "{:?}",
                inspector.composer.outcome
            );
            assert_eq!(inspector.composer.field.read(cx).value(), "");
            assert!(!inspector.composer.visible);
            assert!(
                inspector.monitor.read(cx).reads() > reads,
                "a commit must trigger an immediate re-read"
            );
        });
        assert_eq!(git(root.path(), &["status", "--porcelain"]), "");
        assert_eq!(
            git(root.path(), &["log", "-1", "--format=%s"]).trim(),
            "Ship it"
        );
        test.refresh().unwrap();
        assert!(visual.debug_bounds("git-operation-success").is_some());
    }

    #[test]
    fn a_failed_push_shows_gits_own_explanation() {
        let root = repo();
        let data = tempfile::tempdir().unwrap();
        // A remote that does not exist: `git push` fails and says why.
        git(
            root.path(),
            &["remote", "add", "origin", "/nonexistent/flowx/remote.git"],
        );
        let mut test = gpui::TestAppContext::single();
        let (inspector, window) = mount(&mut test, data.path(), Some(root.path()), 360.0);
        test.read(|cx| assert!(inspector.read(cx).repository(cx).unwrap().can_push()));
        inspector.update(&mut test, |inspector, cx| inspector.push(cx));
        wait_until(&mut test, &inspector, |i| i.composer.running.is_none());
        test.read(|cx| match &inspector.read(cx).composer.outcome {
            Some(Outcome::Failed(Operation::Push, text)) => {
                assert!(text.contains("/nonexistent/flowx/remote.git"), "{text}");
            }
            other => panic!("expected a push failure, got {other:?}"),
        });
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        assert!(visual.debug_bounds("git-operation-error").is_some());
    }
}
