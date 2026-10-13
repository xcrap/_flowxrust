//! A compact working-tree review inside the conversation column.
//!
//! These are the active project's live Git changes, not a claim that this chat
//! authored every edit. No commit, stage, restore, fetch, or push is performed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fx_core::GitFileStatus;
use fx_design::components::{IconButtonProps, icon_button};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{
    BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, Typography, global,
};
use gpui::prelude::*;
use gpui::{AnyElement, AnyView, App, Context, Entity, SharedString, Task, Window, div, px};
use uuid::Uuid;

use crate::inspector::{PreviewRowStyle, preview_row_style, repository};
use crate::state::AppState;
use repository::{LineKind, Preview, Selection, Snapshot};

#[derive(Clone)]
struct ChangesHandle(Entity<ConversationChanges>);
impl gpui::Global for ChangesHandle {}

/// Kept independently of the right inspector so closing that panel cannot stop
/// status refreshes or make inline previews disappear.
struct ConversationChanges {
    state: Entity<AppState>,
    source: Option<(Uuid, String)>,
    generation: u64,
    selection_generation: u64,
    was_busy: bool,
    expanded: bool,
    snapshot: Option<Arc<Snapshot>>,
    selected: Option<GitFileStatus>,
    preview: Option<Arc<Preview>>,
    error: Option<String>,
    preview_error: Option<String>,
    loading: bool,
    preview_loading: bool,
    read_task: Option<Task<()>>,
    preview_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    scroll: gpui::ScrollHandle,
    preview_scroll: gpui::ScrollHandle,
    focus: Option<gpui::FocusHandle>,
}

pub(crate) fn view(state: Entity<AppState>, cx: &mut App) -> AnyElement {
    let existing = cx
        .try_global::<ChangesHandle>()
        .map(|handle| handle.0.clone())
        .filter(|changes| changes.read(cx).state == state);
    let changes = existing.unwrap_or_else(|| {
        let changes = cx.new(|_| ConversationChanges::new(state));
        cx.set_global(ChangesHandle(changes.clone()));
        changes
    });
    changes.update(cx, |changes, cx| changes.sync(cx));
    AnyView::from(changes).into_any_element()
}

impl ConversationChanges {
    fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            source: None,
            generation: 0,
            selection_generation: 0,
            was_busy: false,
            expanded: false,
            snapshot: None,
            selected: None,
            preview: None,
            error: None,
            preview_error: None,
            loading: false,
            preview_loading: false,
            read_task: None,
            preview_task: None,
            poll_task: None,
            scroll: gpui::ScrollHandle::new(),
            preview_scroll: gpui::ScrollHandle::new(),
            focus: None,
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let source = state.active_agent().and_then(|agent| {
            state
                .active_project()
                .map(|project| (agent.id, project.root_path.clone()))
        });
        let busy = state.is_busy();
        if self.source != source {
            self.source = source;
            self.generation += 1;
            self.read_task = None;
            self.loading = false;
            self.snapshot = None;
            self.error = None;
            self.expanded = false;
            self.clear_selection();
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

    fn clear_selection(&mut self) {
        self.selection_generation += 1;
        self.selected = None;
        self.preview = None;
        self.preview_error = None;
        self.preview_loading = false;
        self.preview_task = None;
        self.preview_scroll
            .set_offset(gpui::point(px(0.0), px(0.0)));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some((_, root)) = self.source.clone() else {
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
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(snapshot) => this.install_snapshot(snapshot, cx),
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn install_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.error = None;
        let selection = self.selected.as_ref().and_then(|selected| {
            snapshot
                .repository
                .as_ref()?
                .files
                .iter()
                .find(|file| file.path == selected.path)
                .cloned()
        });
        self.snapshot = Some(Arc::new(snapshot));
        if let Some(selected) = selection {
            self.selected = Some(selected.clone());
            if self.expanded {
                self.load_preview(selected, cx);
            }
        } else {
            self.clear_selection();
        }
    }

    fn select(&mut self, selection: GitFileStatus, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|file| file.path == selection.path)
        {
            self.clear_selection();
        } else {
            self.clear_selection();
            self.load_preview(selection, cx);
        }
        cx.notify();
    }

    fn load_preview(&mut self, selected: GitFileStatus, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let root = snapshot.root.clone();
        self.selection_generation += 1;
        let ticket = (self.generation, self.selection_generation);
        self.selected = Some(selected.clone());
        self.preview_loading = true;
        self.preview_error = None;
        let task = repository::spawn_read(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(repository::preview(&root, &Selection::Change(selected)))
        });
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_read(task).await;
            let _ = this.update(cx, |this, cx| {
                if ticket != (this.generation, this.selection_generation) {
                    return;
                }
                this.preview_loading = false;
                match result {
                    Ok(preview) => this.preview = Some(Arc::new(preview)),
                    Err(error) => {
                        this.preview = None;
                        this.preview_error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn preview_view(&self) -> AnyElement {
        if let Some(error) = &self.preview_error {
            return note(error, true);
        }
        let Some(preview) = &self.preview else {
            return note("Reading diff…", false);
        };
        let theme = global::current();
        let mut lines = div()
            .debug_selector(|| "conversation-changes-diff".into())
            .min_w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .pb(px_len(Spacing::SM));
        if let Some(notice) = &preview.notice {
            lines = lines.child(note(notice, false));
        }
        if preview.lines.is_empty() {
            lines = lines.child(note("Empty file.", false));
        }
        for (index, line) in inline_preview_lines(preview) {
            let style = preview_row_style(line, true);
            let (fg, bg) = match style {
                PreviewRowStyle::Section => (Colors::fg(&theme), Colors::bg_elevated(&theme)),
                PreviewRowStyle::Metadata => {
                    (Colors::fg_tertiary(&theme), Colors::bg_elevated(&theme))
                }
                PreviewRowStyle::Hunk => (Colors::fg_secondary(&theme), Colors::bg_surface(&theme)),
                PreviewRowStyle::Code => match line.kind {
                    LineKind::Added => {
                        (Colors::diff_added_fg(&theme), Colors::diff_added_bg(&theme))
                    }
                    LineKind::Removed => (
                        Colors::diff_removed_fg(&theme),
                        Colors::diff_removed_bg(&theme),
                    ),
                    _ => (Colors::fg(&theme), Colors::content_bg(&theme)),
                },
            };
            lines = lines.child(
                div()
                    .debug_selector(move || format!("conversation-changes-line:{index}"))
                    .min_w_full()
                    .flex_shrink_0()
                    .px(px_len(Spacing::MD))
                    .py(px_len(Spacing::XXXS))
                    .fx_role(if style == PreviewRowStyle::Section {
                        TextRole::CaptionMedium
                    } else {
                        TextRole::MonoSmall
                    })
                    .whitespace_nowrap()
                    .text_color(fg)
                    .bg(bg)
                    .child(SharedString::from(if line.text.is_empty() {
                        " ".into()
                    } else {
                        line.text.clone()
                    })),
            );
        }
        div()
            .id("conversation-changes-diff-scroll")
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .overflow_x_scroll()
            .track_scroll(&self.preview_scroll)
            .flex()
            .items_start()
            .border_y(px_len(BorderWidth::HAIRLINE))
            .border_color(Colors::border_subtle(&theme))
            .child(lines)
            .into_any_element()
    }

    fn files_view(&self, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let mut list = div()
            .id("conversation-changes-files")
            .debug_selector(|| "conversation-changes-files".into())
            .max_h(px(Layout::DIFF_NAVIGATOR_COMPACT_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col();
        if let Some(error) = &self.error {
            list = list.child(note(error, true));
        }
        if let Some(notice) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.notice.as_ref())
        {
            list = list.child(note(notice, false));
        }
        list = list.child(note(
            "Live project working tree · Includes changes from other chats and editors",
            false,
        ));
        for file in self
            .snapshot
            .iter()
            .filter_map(|snapshot| snapshot.repository.as_ref())
            .flat_map(|repo| &repo.files)
        {
            let selected = self
                .selected
                .as_ref()
                .is_some_and(|selected| selected.path == file.path);
            let selection = file.clone();
            let full_path = file.path.clone();
            let mut row = div()
                .id(SharedString::from(format!(
                    "conversation-change:{}",
                    file.path
                )))
                .debug_selector({
                    let path = file.path.clone();
                    move || format!("conversation-change:{path}")
                })
                .w_full()
                .flex_shrink_0()
                .min_h(px_len(Layout::CONTROL_HEIGHT))
                .flex()
                .items_center()
                .gap(px_len(Spacing::SM))
                .px(px_len(Spacing::MD))
                .cursor_pointer()
                .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
                .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                .tooltip(move |window, cx| {
                    fx_design::components::tooltip(full_path.clone(), window, cx)
                })
                .child(
                    (if selected {
                        Icon::CHEVRON_DOWN
                    } else {
                        Icon::CHEVRON_RIGHT
                    })
                    .render(
                        IconSize::Micro,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .fx_caption()
                        .text_color(Colors::fg(&theme))
                        .child(SharedString::from(file.path.clone())),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .fx_role(TextRole::MonoSmall)
                        .text_color(Colors::fg_tertiary(&theme))
                        .child(SharedString::from(file.status())),
                );
            if selected {
                row = row.bg(Colors::bg_selected(&theme));
            }
            list = list.child(row);
            if selected {
                list = list.child(self.preview_view());
            }
        }
        list.into_any_element()
    }
}

impl Render for ConversationChanges {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self
            .focus
            .get_or_insert_with(|| {
                let mut focus = cx.focus_handle();
                focus.tab_stop = true;
                focus
            })
            .clone();
        if self.source.is_none() {
            return div().into_any_element();
        }
        let repo = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.repository.as_ref());
        // No Git integration is advertised for an ordinary folder. Errors still
        // get a visible retry affordance rather than looking like a clean tree.
        if self.snapshot.is_some() && repo.is_none() && self.error.is_none() {
            return div().into_any_element();
        }
        let theme = global::current();
        let count = repo.map_or(0, |repo| repo.files.len());
        let label = if self.error.is_some() {
            "Working tree unavailable".into()
        } else if self.snapshot.is_none() {
            "Reading working tree…".into()
        } else if count == 0 {
            "Working tree clean".into()
        } else {
            format!(
                "Working tree · {count} {}",
                if count == 1 { "file" } else { "files" }
            )
        };
        let can_expand = count > 0 || self.error.is_some();
        let mut header = div()
            .id("conversation-changes-toggle")
            .debug_selector(|| "conversation-changes-toggle".into())
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px_len(Spacing::SM))
            .min_h(px_len(Layout::CONTROL_HEIGHT))
            .px(px_len(Spacing::SM))
            .rounded(px_len(Radii::SM))
            .child(
                (if self.expanded {
                    Icon::CHEVRON_DOWN
                } else {
                    Icon::CHEVRON_RIGHT
                })
                .render(IconSize::Micro, Colors::fg_tertiary(&theme), &theme),
            )
            .child(Icon::ARROW_TRIANGLE_BRANCH.render(
                IconSize::Small,
                Colors::fg_tertiary(&theme),
                &theme,
            ))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .fx_role(TextRole::CaptionMedium)
                    .text_color(Colors::fg_secondary(&theme))
                    .child(SharedString::from(label)),
            );
        if can_expand {
            header = header
                .cursor_pointer()
                .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.expanded = !this.expanded;
                    if this.expanded {
                        this.refresh(cx);
                    }
                    cx.notify();
                }));
        }
        if let Some(repo) = repo.filter(|repo| !repo.branch.is_empty()) {
            header = header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child(SharedString::from(repo.branch.clone())),
            );
        }
        let mut card = div()
            .debug_selector(|| "conversation-changes-card".into())
            .w_full()
            .min_w_0()
            .rounded(px_len(Radii::MD))
            .border(px_len(BorderWidth::HAIRLINE))
            .border_color(Colors::border_subtle(&theme))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px_len(Spacing::XS))
                    .py(px_len(Spacing::XXXS))
                    .child(header)
                    .child(icon_button(
                        IconButtonProps::new(Icon::ARROW_CLOCKWISE, "Refresh working tree")
                            .small()
                            .ghost()
                            .enabled(!self.loading),
                        cx.listener(|this, _, _, cx| this.refresh(cx)),
                    )),
            );
        if self.expanded && can_expand {
            card = card.child(self.files_view(cx));
        }
        div()
            .id("conversation-changes")
            .debug_selector(|| "conversation-changes".into())
            .track_focus(&focus)
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                window.focus(&focus)
            })
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.expanded && scroll_with_key(&this.scroll, &this.preview_scroll, event) {
                    window.refresh();
                    cx.stop_propagation();
                }
            }))
            .w_full()
            .flex_shrink_0()
            .flex()
            .justify_center()
            .pb(px_len(Spacing::SM))
            .child(
                div()
                    .w_full()
                    .max_w(px(Layout::READABLE_CONTENT_WIDTH))
                    .px(px_len(Spacing::LG))
                    .child(card),
            )
            .into_any_element()
    }
}

/// The file row already gives the path. Keep this compact surface focused on
/// changes, retaining Git's meaningful rename/mode/binary notices and all source
/// lines. Within hunks (or a new file), text resembling a header is still code.
fn inline_preview_lines(
    preview: &Preview,
) -> impl Iterator<Item = (usize, &repository::PreviewLine)> {
    let mut source = false;
    preview.lines.iter().enumerate().filter(move |(_, line)| {
        if matches!(line.text.as_str(), "Staged changes" | "Unstaged changes")
            || line.text.starts_with("diff --git ")
        {
            source = false;
        } else if line.text.starts_with("@@") || line.text == "Untracked file" {
            source = true;
        }
        source
            || !["diff --git ", "index ", "--- ", "+++ "]
                .iter()
                .any(|prefix| line.text.starts_with(prefix))
    })
}

fn scroll_with_key(
    vertical: &gpui::ScrollHandle,
    horizontal: &gpui::ScrollHandle,
    event: &gpui::KeyDownEvent,
) -> bool {
    let line = Typography::body_point_size(&global::current()) * 3.0;
    let offset = vertical.offset();
    let page = vertical.bounds().size.height * 0.9;
    let next = match event.keystroke.key.as_str() {
        "up" => gpui::point(offset.x, offset.y + px(line)),
        "down" => gpui::point(offset.x, offset.y - px(line)),
        "pageup" => gpui::point(offset.x, offset.y + page),
        "pagedown" => gpui::point(offset.x, offset.y - page),
        "home" => gpui::point(offset.x, px(0.0)),
        "end" => gpui::point(offset.x, -vertical.max_offset().height),
        "left" | "right" => {
            let offset = horizontal.offset();
            let step = if event.keystroke.key == "left" {
                line
            } else {
                -line
            };
            horizontal.set_offset(gpui::point(offset.x + px(step), offset.y));
            return true;
        }
        _ => return false,
    };
    vertical.set_offset(next);
    true
}

fn note(text: &str, error: bool) -> AnyElement {
    let theme = global::current();
    div()
        .min_w_0()
        .flex_shrink_0()
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .fx_caption()
        .text_color(if error {
            Colors::error(&theme)
        } else {
            Colors::fg_tertiary(&theme)
        })
        .child(SharedString::from(text.to_string()))
        .into_any_element()
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

    fn snapshot(root: &std::path::Path) -> Snapshot {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(repository::changes_snapshot(root))
            .unwrap()
    }

    fn wait_for_preview(test: &mut gpui::TestAppContext, changes: &Entity<ConversationChanges>) {
        for _ in 0..200 {
            test.run_until_parked();
            if test.read(|cx| !changes.read(cx).preview_loading) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        test.read(|cx| {
            let changes = changes.read(cx);
            assert!(!changes.preview_loading, "diff reader timed out");
            assert!(
                changes.preview_error.is_none(),
                "{:?}",
                changes.preview_error
            );
        });
    }

    #[test]
    fn inline_diff_reads_real_staged_unstaged_and_new_files_without_the_inspector() {
        let temp = repo();
        std::fs::write(temp.path().join("sample.txt"), "staged\n").unwrap();
        git(temp.path(), &["add", "sample.txt"]);
        std::fs::write(temp.path().join("sample.txt"), "unstaged\n").unwrap();
        std::fs::write(temp.path().join("new.txt"), "new contents\n").unwrap();
        let before_index = std::fs::read(temp.path().join(".git/index")).unwrap();
        let snapshot = snapshot(temp.path());
        assert_eq!(snapshot.repository.as_ref().unwrap().files.len(), 2);
        let state_dir = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = test.update(|cx| {
            cx.new(|cx| AppState::restore(fx_store::AppPaths::at(state_dir.path()), cx))
        });
        state.update(&mut test, |state, _| {
            state.snapshot.right_panel_visible = false
        });
        let changes = test.update(|cx| {
            cx.new(|_| {
                let mut changes = ConversationChanges::new(state.clone());
                changes.source = Some((Uuid::new_v4(), temp.path().to_string_lossy().into_owned()));
                changes.snapshot = Some(Arc::new(snapshot));
                changes.expanded = true;
                changes
            })
        });
        let mounted = changes.clone();
        let window = test.add_window(move |_, _| TestView(mounted));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(gpui::size(px(760.0), px(500.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let row = visual
            .debug_bounds("conversation-change:sample.txt")
            .unwrap()
            .center();
        visual.simulate_mouse_down(row, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(row, gpui::MouseButton::Left, Default::default());
        wait_for_preview(&mut test, &changes);
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(visual.debug_bounds("conversation-changes-diff").is_some());
        test.read(|cx| {
            assert!(!state.read(cx).snapshot.right_panel_visible);
            let text = &changes.read(cx).preview.as_ref().unwrap().text;
            assert!(text.contains("Staged changes") && text.contains("Unstaged changes"));
            assert!(text.contains("+staged") && text.contains("+unstaged"));
        });
        changes.update(&mut test, |changes, cx| {
            changes.select(GitFileStatus::new("new.txt", "?", "?"), cx);
            assert!(
                changes.preview.is_none(),
                "the old file's diff must clear immediately"
            );
        });
        wait_for_preview(&mut test, &changes);
        test.read(|cx| {
            assert!(
                changes
                    .read(cx)
                    .preview
                    .as_ref()
                    .unwrap()
                    .text
                    .contains("+new contents")
            )
        });
        assert_eq!(
            before_index,
            std::fs::read(temp.path().join(".git/index")).unwrap()
        );
    }

    #[test]
    fn refresh_updates_a_diff_even_when_git_status_letters_do_not_change() {
        let temp = repo();
        std::fs::write(temp.path().join("sample.txt"), "first edit\n").unwrap();
        let initial = snapshot(temp.path());
        let state_dir = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = test.update(|cx| {
            cx.new(|cx| AppState::restore(fx_store::AppPaths::at(state_dir.path()), cx))
        });
        let changes = test.update(|cx| {
            cx.new(|_| {
                let mut changes = ConversationChanges::new(state);
                changes.source = Some((Uuid::new_v4(), temp.path().to_string_lossy().into_owned()));
                changes.snapshot = Some(Arc::new(initial));
                changes.expanded = true;
                changes
            })
        });
        changes.update(&mut test, |changes, cx| {
            changes.select(GitFileStatus::new("sample.txt", " ", "M"), cx)
        });
        wait_for_preview(&mut test, &changes);
        std::fs::write(temp.path().join("sample.txt"), "second edit\n").unwrap();
        let updated = snapshot(temp.path());
        changes.update(&mut test, |changes, cx| {
            changes.install_snapshot(updated, cx)
        });
        wait_for_preview(&mut test, &changes);
        test.read(|cx| {
            let preview = changes.read(cx).preview.as_ref().unwrap();
            assert!(preview.text.contains("+second edit"));
            assert!(!preview.text.contains("+first edit"));
        });
        std::fs::write(temp.path().join("sample.txt"), "original\n").unwrap();
        let clean = snapshot(temp.path());
        changes.update(&mut test, |changes, cx| {
            changes.install_snapshot(clean, cx);
            assert!(changes.selected.is_none());
            assert!(changes.preview.is_none());
        });
    }

    #[test]
    fn switching_threads_discards_inflight_previews_and_selection() {
        let temp = repo();
        std::fs::write(temp.path().join("sample.txt"), "first edit\n").unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = test.update(|cx| {
            cx.new(|cx| AppState::restore(fx_store::AppPaths::at(state_dir.path()), cx))
        });
        state.update(&mut test, |state, _| {
            state.add_project(&temp.path().to_string_lossy());
            state.create_thread();
        });
        let changes = test.update(|cx| cx.new(|_| ConversationChanges::new(state.clone())));
        changes.update(&mut test, |changes, cx| {
            changes.sync(cx);
            changes.install_snapshot(snapshot(temp.path()), cx);
            changes.expanded = true;
            changes.select(GitFileStatus::new("sample.txt", " ", "M"), cx);
            assert!(changes.preview_loading);
        });
        state.update(&mut test, |state, _| {
            state.create_thread();
        });
        changes.update(&mut test, |changes, cx| {
            let previous_generation = changes.generation;
            changes.sync(cx);
            assert!(changes.generation > previous_generation);
            assert!(changes.selected.is_none());
            assert!(changes.preview.is_none());
            assert!(!changes.expanded);
        });
        for _ in 0..20 {
            test.run_until_parked();
            std::thread::sleep(Duration::from_millis(5));
        }
        test.read(|cx| assert!(changes.read(cx).preview.is_none()));
    }

    struct TestView(Entity<ConversationChanges>);
    impl Render for TestView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w_full().h_full().child(self.0.clone())
        }
    }

    #[test]
    fn large_inline_diffs_keep_line_height_and_support_keyboard_scrolling() {
        let temp = repo();
        std::fs::write(
            temp.path().join("sample.txt"),
            (0..100)
                .map(|index| format!("line {index} {}\n", "wide source ".repeat(30)))
                .collect::<String>(),
        )
        .unwrap();
        let snapshot = snapshot(temp.path());
        let state_dir = tempfile::tempdir().unwrap();
        let mut test = gpui::TestAppContext::single();
        let state = test.update(|cx| {
            cx.new(|cx| AppState::restore(fx_store::AppPaths::at(state_dir.path()), cx))
        });
        let changes = test.update(|cx| {
            cx.new(|_| {
                let mut changes = ConversationChanges::new(state);
                changes.source = Some((Uuid::new_v4(), temp.path().to_string_lossy().into_owned()));
                changes.snapshot = Some(Arc::new(snapshot));
                changes.expanded = true;
                changes
            })
        });
        changes.update(&mut test, |changes, cx| {
            changes.select(GitFileStatus::new("sample.txt", " ", "M"), cx)
        });
        wait_for_preview(&mut test, &changes);
        let mounted = changes.clone();
        let window = test.add_window(move |_, _| TestView(mounted));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        visual.simulate_resize(gpui::size(px(500.0), px(500.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let card = visual.debug_bounds("conversation-changes-card").unwrap();
        assert_eq!(card.size.width, px(500.0 - Spacing::LG * 2.0));
        assert_eq!(card.left(), px(Spacing::LG));
        let viewport = visual.debug_bounds("conversation-changes-files").unwrap();
        assert_eq!(
            viewport.size.height,
            px(Layout::DIFF_NAVIGATOR_COMPACT_HEIGHT)
        );
        assert!(
            visual
                .debug_bounds("conversation-changes-line:9")
                .unwrap()
                .size
                .height
                >= px(18.0)
        );
        test.read(|cx| {
            assert!(changes.read(cx).scroll.max_offset().height > px(1_000.0));
            assert!(changes.read(cx).preview_scroll.max_offset().width > px(1_000.0));
        });
        visual.simulate_mouse_down(
            viewport.center(),
            gpui::MouseButton::Left,
            Default::default(),
        );
        visual.simulate_mouse_up(
            viewport.center(),
            gpui::MouseButton::Left,
            Default::default(),
        );
        visual.simulate_keystrokes("pagedown right");
        test.refresh().unwrap();
        test.update(|_| {});
        test.read(|cx| {
            assert!(changes.read(cx).scroll.offset().y < px(0.0));
            assert!(changes.read(cx).preview_scroll.offset().x < px(0.0));
        });
        visual.simulate_resize(gpui::size(px(1_160.0), px(500.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let card = visual.debug_bounds("conversation-changes-card").unwrap();
        assert_eq!(
            card.size.width,
            px(Layout::READABLE_CONTENT_WIDTH - Spacing::LG * 2.0)
        );
        assert_eq!(card.center().x, px(580.0));
    }

    #[test]
    fn inline_preview_hides_transport_headers_but_keeps_change_notices_and_source() {
        let text = [
            "Staged changes",
            "diff --git a/a b/a",
            "index 111..222 100644",
            "--- a/a",
            "+++ b/a",
            "@@ -1 +1 @@",
            "-old",
            "+new",
            "+++ source text",
            "Unstaged changes",
            "diff --git a/a b/b",
            "index 222..333 100644",
            "old mode 100644",
            "new mode 100755",
            "rename from a",
            "rename to b",
            "Binary files a/a and b/b differ",
        ];
        let preview = Preview {
            text: text.join("\n"),
            lines: text
                .iter()
                .map(|text| repository::PreviewLine {
                    text: text.to_string(),
                    kind: LineKind::Context,
                })
                .collect(),
            notice: None,
        };
        let shown: Vec<_> = inline_preview_lines(&preview)
            .map(|(_, line)| line.text.as_str())
            .collect();
        assert_eq!(
            shown,
            [
                "Staged changes",
                "@@ -1 +1 @@",
                "-old",
                "+new",
                "+++ source text",
                "Unstaged changes",
                "old mode 100644",
                "new mode 100755",
                "rename from a",
                "rename to b",
                "Binary files a/a and b/b differ",
            ]
        );
        let preview = Preview {
            text: String::new(),
            lines: ["Untracked file", "+++ new file source"]
                .iter()
                .map(|text| repository::PreviewLine {
                    text: text.to_string(),
                    kind: LineKind::Added,
                })
                .collect(),
            notice: None,
        };
        assert_eq!(inline_preview_lines(&preview).count(), 2);
    }
}
