//! Live, read-only changes and files inspector.

#[path = "git_inspector.rs"]
mod repository;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fx_design::components::{
    DividerAxis, IconButtonProps, SegmentedOption, divider_with_axis, icon_button,
    segmented_control,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{Colors, Spacing, TextRole, global};
use fx_store::RightPanelTab;
use gpui::prelude::*;
use gpui::{
    AnyElement, AnyView, App, ClipboardItem, Context, Entity, SharedString, Task, Window, div, px,
};

use crate::state::AppState;
use repository::{LineKind, Preview, Selection, Snapshot};

const INSPECTOR_HEADER_HEIGHT: f32 = 44.0;

#[derive(Clone)]
struct InspectorHandle(Entity<Inspector>);
impl gpui::Global for InspectorHandle {}

struct Inspector {
    state: Entity<AppState>,
    project: Option<String>,
    tab: RightPanelTab,
    active: bool,
    generation: u64,
    selection_generation: u64,
    snapshot: Option<Arc<Snapshot>>,
    selected: Option<Selection>,
    preview: Option<Arc<Preview>>,
    error: Option<String>,
    preview_error: Option<String>,
    loading: bool,
    preview_loading: bool,
    read_task: Option<Task<()>>,
    preview_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
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
        let inspector = cx.new(|_| Inspector::new(state.clone()));
        cx.set_global(InspectorHandle(inspector.clone()));
        inspector
    });
    inspector.update(cx, |inspector, cx| inspector.sync(cx));
    AnyView::from(inspector).into_any_element()
}

impl Inspector {
    fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            project: None,
            tab: RightPanelTab::Changes,
            active: true,
            generation: 0,
            selection_generation: 0,
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
        }
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        let project = self
            .state
            .read(cx)
            .active_project()
            .map(|project| project.root_path.clone());
        let tab = self.state.read(cx).snapshot.right_panel_tab;
        if self.project != project {
            self.project = project;
            self.generation += 1;
            self.snapshot = None;
            self.error = None;
            self.read_task = None;
            self.loading = false;
            self.clear_selection();
            self.refresh(cx);
            cx.notify();
        }
        if self.tab != tab {
            self.tab = tab;
            self.clear_selection();
            cx.notify();
        }
        if self.poll_task.is_none() {
            self.poll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    gpui::Timer::after(Duration::from_secs(5)).await;
                    if this
                        .update(cx, |this, cx| {
                            if this.active && !this.loading {
                                this.refresh(cx);
                            }
                        })
                        .is_err()
                    {
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
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
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
            runtime.block_on(repository::snapshot(&PathBuf::from(project)))
        });
        self.read_task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_read(task).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(snapshot) => {
                        this.error = None;
                        this.snapshot = Some(Arc::new(snapshot));
                        if let Some(selected) = this.selected.clone() {
                            let snapshot = this.snapshot.as_ref().unwrap();
                            let refreshed = match selected {
                                Selection::File(path) if snapshot.files.contains(&path) => {
                                    Some(Selection::File(path))
                                }
                                Selection::Change(status) => snapshot
                                    .repository
                                    .as_ref()
                                    .and_then(|repo| {
                                        repo.files.iter().find(|file| file.path == status.path)
                                    })
                                    .cloned()
                                    .map(Selection::Change),
                                _ => None,
                            };
                            if let Some(selected) = refreshed {
                                this.select(selected, cx);
                            } else {
                                this.clear_selection();
                            }
                        }
                    }
                    Err(error) => {
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let root = snapshot.root.clone();
        self.selection_generation += 1;
        let ticket = (self.generation, self.selection_generation);
        if self.selected.as_ref() != Some(&selection) {
            self.preview = None;
        }
        self.selected = Some(selection.clone());
        self.preview_error = None;
        self.preview_loading = true;
        let task = repository::spawn_read(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(repository::preview(&root, &selection))
        });
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_read(task).await;
            let _ = this.update(cx, |this, cx| {
                if !this.accepts_preview(ticket) {
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
        cx.notify();
    }

    fn accepts_preview(&self, ticket: (u64, u64)) -> bool {
        ticket == (self.generation, self.selection_generation)
    }

    fn header(&self, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let options = [
            SegmentedOption::new(RightPanelTab::Changes, "Changes"),
            SegmentedOption::new(RightPanelTab::Files, "Files"),
        ];
        let for_tabs = self.state.clone();
        let for_hide = self.state.clone();
        let heading = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.repository.as_ref())
            .map(|repo| repo.branch.clone())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| {
                self.state
                    .read(cx)
                    .active_project()
                    .map(|project| project.name.clone())
                    .unwrap_or_else(|| "No project".into())
            });
        div()
            .h(px(INSPECTOR_HEADER_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px_len(Spacing::SM))
            .px(px_len(Spacing::MD))
            .child(segmented_control(&options, &self.tab, move |tab, _, cx| {
                for_tabs.update(cx, |state, cx| {
                    state.set_inspector_tab(*tab);
                    cx.notify();
                });
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(SharedString::from(heading)),
            )
            .child(icon_button(
                IconButtonProps::new(Icon::ARROW_CLOCKWISE, "Refresh files and changes")
                    .small()
                    .ghost()
                    .enabled(self.project.is_some() && !self.loading),
                cx.listener(|this, _, _, cx| this.refresh(cx)),
            ))
            .child(icon_button(
                IconButtonProps::new(Icon::SIDEBAR_RIGHT, "Hide inspector")
                    .small()
                    .ghost(),
                move |_, _, cx| {
                    for_hide.update(cx, |state, cx| {
                        state.toggle_inspector();
                        cx.notify();
                    });
                },
            ))
            .into_any_element()
    }

    fn file_list(&self, cx: &Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.snapshot else {
            return note(
                if self.project.is_none() {
                    "Select a project to browse its files."
                } else if self.loading {
                    "Reading project…"
                } else {
                    "Use Refresh to read this project."
                },
                false,
            );
        };
        let selections = match self.tab {
            RightPanelTab::Changes => {
                let Some(repo) = &snapshot.repository else {
                    return note(
                        "This folder has no Git repository. Its files are available in the Files tab.",
                        false,
                    );
                };
                if repo.files.is_empty() {
                    return note("Working tree is clean.", false);
                }
                repo.files
                    .iter()
                    .cloned()
                    .map(Selection::Change)
                    .collect::<Vec<_>>()
            }
            RightPanelTab::Files => snapshot
                .files
                .iter()
                .cloned()
                .map(Selection::File)
                .collect(),
        };
        if selections.is_empty() {
            return note("This folder contains no visible files.", false);
        }
        let theme = global::current();
        let mut list = div()
            .id("inspector.file-list")
            .min_h_0()
            .overflow_y_scroll()
            .py(px_len(Spacing::XXS));
        list = if self.selected.is_some() {
            list.max_h(px(220.0)).flex_shrink_0()
        } else {
            list.flex_1()
        };
        for selection in selections {
            let selected = self.selected.as_ref() == Some(&selection);
            let path = selection.path().to_string();
            let status = match &selection {
                Selection::Change(status) => status.status(),
                _ => String::new(),
            };
            let mut row = div()
                .id(SharedString::from(format!("inspector-file:{path}")))
                .h(px(28.0))
                .flex()
                .items_center()
                .gap(px_len(Spacing::SM))
                .px(px_len(Spacing::MD))
                .cursor_pointer()
                .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
                .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                .child(
                    div()
                        .w(px(26.0))
                        .flex_shrink_0()
                        .fx_mono()
                        .text_color(Colors::fg_tertiary(&theme))
                        .child(SharedString::from(status)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .fx_caption()
                        .text_color(Colors::fg(&theme))
                        .child(SharedString::from(path)),
                );
            if selected {
                row = row.bg(Colors::bg_selected(&theme));
            }
            list = list.child(row);
        }
        list.into_any_element()
    }

    fn preview_view(&self, cx: &Context<Self>) -> AnyElement {
        let Some(selected) = &self.selected else {
            return div().into_any_element();
        };
        let theme = global::current();
        let text = self
            .preview
            .as_ref()
            .map(|preview| preview.text.clone())
            .unwrap_or_default();
        let path = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.root.join(selected.path()));
        let mut body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(divider_with_axis(DividerAxis::Horizontal))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px_len(Spacing::XS))
                    .px(px_len(Spacing::MD))
                    .py(px_len(Spacing::XXS))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .fx_role(TextRole::CaptionMedium)
                            .text_color(Colors::fg(&theme))
                            .child(SharedString::from(selected.path().to_string())),
                    )
                    .child(icon_button(
                        IconButtonProps::new(Icon::DOC_ON_DOC, "Copy preview")
                            .small()
                            .ghost()
                            .enabled(self.preview.is_some()),
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                        },
                    ))
                    .child(icon_button(
                        IconButtonProps::new(Icon::FOLDER, "Reveal file in Finder")
                            .small()
                            .ghost(),
                        move |_, _, cx| {
                            if let Some(path) = &path {
                                cx.reveal_path(path);
                            }
                        },
                    ))
                    .child(icon_button(
                        IconButtonProps::new(Icon::XMARK, "Close preview")
                            .small()
                            .ghost(),
                        cx.listener(|this, _, _, cx| {
                            this.clear_selection();
                            cx.notify();
                        }),
                    )),
            );
        if let Some(error) = &self.preview_error {
            return body.child(note(error, true)).into_any_element();
        }
        let Some(preview) = &self.preview else {
            return body
                .child(note("Loading preview…", false))
                .into_any_element();
        };
        if let Some(notice) = &preview.notice {
            body = body.child(note(notice, false));
        }
        let mut lines = div()
            .id("inspector.preview")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .flex()
            .flex_col();
        if preview.lines.is_empty() {
            lines = lines.child(note("Empty file.", false));
        }
        for line in &preview.lines {
            let (fg, bg) = match line.kind {
                LineKind::Added => (Colors::diff_added_fg(&theme), Colors::diff_added_bg(&theme)),
                LineKind::Removed => (
                    Colors::diff_removed_fg(&theme),
                    Colors::diff_removed_bg(&theme),
                ),
                LineKind::Header => (Colors::fg_secondary(&theme), Colors::bg_surface(&theme)),
                LineKind::Context => (Colors::fg(&theme), Colors::diff_context_bg(&theme)),
            };
            lines = lines.child(
                div()
                    .flex_shrink_0()
                    .min_w_full()
                    .px(px_len(Spacing::SM))
                    .fx_mono()
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
        body.child(lines).into_any_element()
    }
}

impl Render for Inspector {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let mut view = div()
            .w(px(self.state.read(cx).right_panel_width()))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(Colors::bg_elevated(&theme))
            .border_l_1()
            .border_color(Colors::border_subtle(&theme))
            .child(self.header(cx))
            .child(divider_with_axis(DividerAxis::Horizontal));
        if let Some(error) = &self.error {
            view = view.child(note(error, true));
        }
        if let Some(notice) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.notice.as_ref())
        {
            view = view.child(note(notice, false));
        }
        view.child(self.file_list(cx)).child(self.preview_view(cx))
    }
}

fn note(text: &str, error: bool) -> AnyElement {
    let theme = global::current();
    div()
        .p(px_len(Spacing::MD))
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
    use super::*;

    #[test]
    fn selecting_a_painted_file_row_loads_and_paints_its_contents() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("sample.txt"), "hello from a file\n").unwrap();
        let mut test = gpui::TestAppContext::single();
        let paths = fx_store::AppPaths::at(temp.path().join("state"));
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        let root = temp.path().to_path_buf();
        let window = test.add_window(move |_, _| {
            let mut inspector = Inspector::new(state);
            inspector.tab = RightPanelTab::Files;
            inspector.snapshot = Some(Arc::new(Snapshot {
                root,
                repository: None,
                files: vec!["sample.txt".into()],
                notice: None,
            }));
            inspector
        });
        test.refresh().unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), &test);
        let position = gpui::point(px(100.0), px(60.0));
        visual.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(position, gpui::MouseButton::Left, Default::default());
        // The bounded-stack reader is a real OS worker, outside GPUI's fake
        // dispatcher. Pump delivery while waiting for its completion.
        for _ in 0..200 {
            test.run_until_parked();
            if window
                .update(&mut test, |inspector, _, _| inspector.preview.is_some())
                .unwrap()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        test.refresh().unwrap();
        window
            .update(&mut test, |inspector, _, _| {
                assert_eq!(
                    inspector.selected.as_ref().map(Selection::path),
                    Some("sample.txt")
                );
                assert_eq!(
                    inspector.preview.as_ref().unwrap().text,
                    "hello from a file\n"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn late_previews_cannot_cross_a_project_or_selection_switch(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let paths = fx_store::AppPaths::at(temp.path());
        let state = cx.new(|cx| AppState::restore(paths, cx));
        let inspector = cx.new(|_| Inspector::new(state));
        inspector.update(cx, |inspector, _| {
            inspector.generation = 3;
            inspector.selection_generation = 4;
            assert!(inspector.accepts_preview((3, 4)));
            inspector.clear_selection();
            assert!(!inspector.accepts_preview((3, 4)));
            assert!(inspector.accepts_preview((3, 5)));
            inspector.generation += 1;
            assert!(!inspector.accepts_preview((3, 5)));
        });
    }
}
