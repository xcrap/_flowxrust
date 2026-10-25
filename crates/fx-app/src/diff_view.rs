//! The diff viewer: comparison modes, inline and split layouts, a file
//! navigator, and per-file disclosure.
//!
//! Rows are virtualized. The canvas is one `uniform_list` over a
//! [`DiffRowLayout`], which stores a prefix sum per file and resolves a row to a
//! file and line by binary search, so drawing cost follows what is on screen
//! and not how long the diff is. A file header is one row like any other,
//! which is why every row shares the code rows' height.
//!
//! A refresh never blanks the canvas. The displayed document stays until its
//! replacement has been parsed, and the replacement is scrolled to the same
//! file and line the reader was looking at — a poll every five seconds while an
//! agent edits the open file would otherwise throw them back to the top.

use std::sync::Arc;

use fx_core::GitRepositoryInfo;
use fx_core::git::{DiffLine, DiffLineKind, DiffSection, SplitRow, SplitSide};
use fx_core::policy::{DiffDisclosureState, DiffRowLayout};
use fx_design::components::{
    DividerAxis, IconButtonProps, SegmentedOption, compact_action_toggle, divider_with_axis,
    glyph_button, icon_button, segmented_control,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{
    BorderWidth, Colors, IconSize, Layout, Spacing, TextRole, Theme, Typography, global,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, Context, ListHorizontalSizingBehavior, ScrollStrategy, SharedString, Task,
    UniformListScrollHandle, WeakEntity, div, point, px, relative, uniform_list,
};

use super::repository::{self, ComparisonMode, DiffDocument};
use super::{Inspector, inspector_empty, note, split_file_path};

/// SF Mono's advance is 0.6 em; the extra hundredth absorbs rounding so a
/// line number never clips.
const MONO_ADVANCE: f32 = 0.62;

/// Characters drawn per line. Shaping a minified bundle's megabyte line costs
/// far more than anyone can read of it.
const MAX_DISPLAY_CHARACTERS: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum DiffLayout {
    #[default]
    Inline,
    Split,
}

/// What a displayed document was built from. A document is current only while
/// its key matches the monitor's project generation, the comparison, and the
/// snapshot revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DiffKey {
    pub(super) generation: u64,
    pub(super) mode: ComparisonMode,
    pub(super) revision: u64,
}

/// Where the reader is: a file, an inline line within it, and how far into
/// that row the viewport starts.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ScrollAnchor {
    path: String,
    line: Option<usize>,
    within: f32,
    x: f32,
}

pub(super) struct DiffState {
    pub(super) mode: ComparisonMode,
    pub(super) layout: DiffLayout,
    pub(super) document: Option<Arc<DiffDocument>>,
    pub(super) shown: Option<DiffKey>,
    pub(super) loading: Option<DiffKey>,
    /// The last key whose load failed.
    pub(super) failed: Option<DiffKey>,
    pub(super) error: Option<String>,
    pub(super) disclosure: DiffDisclosureState,
    /// Identified by path, never by its status record: line counts change
    /// on every edit, and the selection must not.
    pub(super) selected_path: Option<String>,
    pub(super) navigator_visible: bool,
    pub(super) scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
}

impl Default for DiffState {
    fn default() -> Self {
        Self {
            mode: ComparisonMode::default(),
            layout: DiffLayout::default(),
            document: None,
            shown: None,
            loading: None,
            failed: None,
            error: None,
            disclosure: DiffDisclosureState::new(),
            selected_path: None,
            navigator_visible: true,
            scroll: UniformListScrollHandle::new(),
            task: None,
        }
    }
}

impl DiffState {
    /// Forget everything tied to one project, keeping the reader's layout
    /// preferences.
    pub(super) fn reset(&mut self) {
        *self = Self {
            mode: self.mode,
            layout: self.layout,
            navigator_visible: self.navigator_visible,
            ..Self::default()
        };
    }

    fn drop_document(&mut self) {
        self.failed = None;
        self.document = None;
        self.shown = None;
        self.loading = None;
        self.task = None;
        self.error = None;
        self.set_offset(0.0, 0.0);
    }

    fn collapsed(&self, document: &DiffDocument) -> Vec<bool> {
        document
            .sections
            .iter()
            .map(|section| self.disclosure.is_collapsed(&section.path))
            .collect()
    }

    pub(super) fn row_layout(&self, document: &DiffDocument) -> DiffRowLayout {
        let collapsed = self.collapsed(document);
        let counts: Vec<usize> = document
            .sections
            .iter()
            .map(|section| rows_in(section, self.layout))
            .collect();
        DiffRowLayout::new(&counts, &|section| collapsed[section])
    }

    fn offset(&self) -> (f32, f32) {
        let offset = self.scroll.0.borrow().base_handle.offset();
        (f32::from(offset.x), f32::from(offset.y))
    }

    fn set_offset(&self, x: f32, y: f32) {
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(x), px(y)));
    }

    /// The file and line at the top of the viewport.
    pub(super) fn anchor(&self) -> Option<ScrollAnchor> {
        let document = self.document.as_ref()?;
        let height = row_height(&global::current());
        let (x, y) = self.offset();
        let top = (-y).max(0.0);
        let row = (top / height).floor() as usize;
        let location = self.row_layout(document).location(row)?;
        let section = &document.sections[location.section];
        Some(ScrollAnchor {
            path: section.path.clone(),
            line: location
                .line
                .map(|line| inline_line_of(section, line, self.layout)),
            within: top - row as f32 * height,
            x,
        })
    }

    /// Scroll the current document back to `anchor`. A file that is gone
    /// leaves the offset alone, which the list clamps.
    pub(super) fn restore(&self, anchor: &ScrollAnchor) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let Some(index) = document
            .sections
            .iter()
            .position(|section| section.path == anchor.path)
        else {
            return;
        };
        let layout = self.row_layout(document);
        let Some(header) = layout.header_row(index) else {
            return;
        };
        let section = &document.sections[index];
        let row = match anchor.line {
            Some(line) if !self.disclosure.is_collapsed(&section.path) => {
                header + 1 + row_of_inline_line(section, line, self.layout)
            }
            _ => header,
        };
        let height = row_height(&global::current());
        // Split rows fit the viewport, so a horizontal offset would only
        // shift them sideways.
        let x = if self.layout == DiffLayout::Split {
            0.0
        } else {
            anchor.x
        };
        self.set_offset(x, -(row as f32 * height + anchor.within));
    }
}

fn rows_in(section: &DiffSection, layout: DiffLayout) -> usize {
    match layout {
        DiffLayout::Inline => section.lines.len(),
        DiffLayout::Split => section.split_rows.len(),
    }
}

/// The inline line a row of `layout` shows.
fn inline_line_of(section: &DiffSection, row: usize, layout: DiffLayout) -> usize {
    match (layout, section.split_rows.get(row)) {
        (DiffLayout::Split, Some(SplitRow::Annotation(line))) => *line,
        (DiffLayout::Split, Some(SplitRow::Pair { old, new })) => old.or(*new).unwrap_or(0),
        _ => row,
    }
}

/// The row of `layout` that shows an inline line, clamped to the section.
fn row_of_inline_line(section: &DiffSection, line: usize, layout: DiffLayout) -> usize {
    let rows = rows_in(section, layout);
    let shows = |row: &SplitRow, test: &dyn Fn(usize) -> bool| match row {
        SplitRow::Annotation(index) => test(*index),
        SplitRow::Pair { old, new } => old.is_some_and(test) || new.is_some_and(test),
    };
    let row = match layout {
        DiffLayout::Inline => line,
        // The row holding the line itself, else the first row after it.
        DiffLayout::Split => section
            .split_rows
            .iter()
            .position(|row| shows(row, &|index| index == line))
            .or_else(|| {
                section
                    .split_rows
                    .iter()
                    .position(|row| shows(row, &|index| index > line))
            })
            .unwrap_or(rows),
    };
    row.min(rows.saturating_sub(1))
}

/// Height of every canvas row, headers included.
pub(super) fn row_height(theme: &Theme) -> f32 {
    Layout::diff_line_height(Typography::terminal_point_size(theme))
}

fn number_width(theme: &Theme, max_line: u32) -> f32 {
    let digits = max_line.max(10).to_string().len() as f32;
    digits * Typography::style(TextRole::MonoSmall, theme).size * MONO_ADVANCE + Spacing::SM * 2.0
}

/// A line as drawn: tabs expanded to stops of four, and bounded in length.
fn visible_text(text: &str) -> SharedString {
    let mut out = String::with_capacity(text.len().min(MAX_DISPLAY_CHARACTERS + 8));
    let mut columns = 0;
    for (count, character) in text.chars().enumerate() {
        if count >= MAX_DISPLAY_CHARACTERS {
            out.push_str(" …");
            break;
        }
        if character == '\t' {
            let stop = 4 - columns % 4;
            out.extend(std::iter::repeat_n(' ', stop));
            columns += stop;
        } else {
            out.push(character);
            columns += 1;
        }
    }
    if out.is_empty() {
        out.push(' ');
    }
    out.into()
}

/// Text and fill for one kind of line.
pub(super) fn line_palette(kind: DiffLineKind, theme: &Theme) -> (gpui::Rgba, gpui::Rgba) {
    match kind {
        DiffLineKind::Meta => (Colors::fg_tertiary(theme), Colors::bg_surface(theme)),
        // Banner text is what is guaranteed legible on a muted wash.
        DiffLineKind::Hunk => (Colors::fg_secondary(theme), Colors::info_muted(theme)),
        DiffLineKind::Context => (Colors::fg_secondary(theme), Colors::content_bg(theme)),
        DiffLineKind::Addition => (Colors::diff_added_fg(theme), Colors::diff_added_bg(theme)),
        DiffLineKind::Deletion => (
            Colors::diff_removed_fg(theme),
            Colors::diff_removed_bg(theme),
        ),
    }
}

fn side_palette(side: SplitSide, theme: &Theme) -> (gpui::Rgba, gpui::Rgba) {
    match side {
        SplitSide::Empty => (Colors::fg_tertiary(theme), Colors::bg_surface(theme)),
        SplitSide::Context => line_palette(DiffLineKind::Context, theme),
        SplitSide::Addition => line_palette(DiffLineKind::Addition, theme),
        SplitSide::Deletion => line_palette(DiffLineKind::Deletion, theme),
    }
}

/// The status letter's colour. A conflict's `U` is the letter alone carrying
/// the state, so it takes `warning_ink`: light-mode `warning` is below 3:1.
pub(super) fn status_ink(status: &str, theme: &Theme) -> gpui::Rgba {
    if status.contains('U') {
        Colors::warning_ink(theme)
    } else if status.contains('D') {
        Colors::error(theme)
    } else if status.contains('A') || status.contains('?') {
        Colors::success(theme)
    } else {
        Colors::fg_secondary(theme)
    }
}

#[derive(Clone, Copy)]
struct RowMetrics {
    height: f32,
    numbers: f32,
}

fn number_cell(
    value: Option<u32>,
    ink: gpui::Rgba,
    metrics: RowMetrics,
    theme: &Theme,
) -> AnyElement {
    div()
        .flex_shrink_0()
        .w(px(metrics.numbers))
        .h_full()
        .flex()
        .items_center()
        .justify_end()
        .px(px_len(Spacing::SM))
        .bg(Colors::bg_elevated(theme))
        .fx_role(TextRole::MonoSmall)
        .text_color(ink)
        .children(value.map(|value| SharedString::from(value.to_string())))
        .into_any_element()
}

fn number_ink(kind: DiffLineKind, theme: &Theme) -> (gpui::Rgba, gpui::Rgba) {
    match kind {
        DiffLineKind::Addition => (Colors::fg_tertiary(theme), Colors::diff_added_fg(theme)),
        DiffLineKind::Deletion => (Colors::diff_removed_fg(theme), Colors::fg_tertiary(theme)),
        _ => (Colors::fg_tertiary(theme), Colors::fg_tertiary(theme)),
    }
}

fn inline_row(line: &DiffLine, metrics: RowMetrics, theme: &Theme) -> AnyElement {
    let (fg, bg) = line_palette(line.kind, theme);
    let (old_ink, new_ink) = number_ink(line.kind, theme);
    div()
        .debug_selector(|| "diff-line".into())
        .h(px(metrics.height))
        .min_w_full()
        .flex()
        .flex_row()
        .items_center()
        .whitespace_nowrap()
        .bg(bg)
        .child(number_cell(line.old_line, old_ink, metrics, theme))
        .child(number_cell(line.new_line, new_ink, metrics, theme))
        .child(
            div()
                .flex_shrink_0()
                .px(px_len(Spacing::SM))
                .fx_role(TextRole::MonoSmall)
                .text_color(fg)
                .child(visible_text(&line.text)),
        )
        .into_any_element()
}

fn split_half(
    section: &DiffSection,
    index: Option<usize>,
    metrics: RowMetrics,
    theme: &Theme,
) -> AnyElement {
    let side = section.split_side(index);
    let (fg, bg) = side_palette(side, theme);
    let line = index.and_then(|index| section.lines.get(index));
    let number = line.and_then(|line| line.old_line.or(line.new_line));
    let ink = match side {
        SplitSide::Addition => Colors::diff_added_fg(theme),
        SplitSide::Deletion => Colors::diff_removed_fg(theme),
        _ => Colors::fg_tertiary(theme),
    };
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .overflow_hidden()
        .bg(bg)
        .child(number_cell(number, ink, metrics, theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .px(px_len(Spacing::SM))
                .truncate()
                .fx_role(TextRole::MonoSmall)
                .text_color(fg)
                .children(line.map(|line| visible_text(line.content()))),
        )
        .into_any_element()
}

fn split_row(section: &DiffSection, row: usize, metrics: RowMetrics, theme: &Theme) -> AnyElement {
    match section.split_rows.get(row) {
        Some(SplitRow::Pair { old, new }) => div()
            .debug_selector(|| "diff-split-row".into())
            .h(px(metrics.height))
            .w_full()
            .flex()
            .child(split_half(section, *old, metrics, theme))
            .child(divider_with_axis(DividerAxis::Vertical))
            .child(split_half(section, *new, metrics, theme))
            .into_any_element(),
        Some(SplitRow::Annotation(index)) => {
            let line = &section.lines[*index];
            let (fg, bg) = line_palette(line.kind, theme);
            div()
                .debug_selector(|| "diff-split-row".into())
                .h(px(metrics.height))
                .w_full()
                .flex()
                .items_center()
                .px(px_len(Spacing::SM))
                .bg(bg)
                .truncate()
                .fx_role(TextRole::MonoSmall)
                .text_color(fg)
                .child(visible_text(&line.text))
                .into_any_element()
        }
        None => div().h(px(metrics.height)).into_any_element(),
    }
}

struct HeaderRow<'a> {
    section: &'a DiffSection,
    collapsed: bool,
    selected: bool,
    /// How far the canvas is scrolled sideways; the label is inset by it so
    /// the file name stays in view.
    pinned: f32,
}

fn header_row(
    header: HeaderRow<'_>,
    metrics: RowMetrics,
    theme: &Theme,
    inspector: WeakEntity<Inspector>,
) -> AnyElement {
    let path = header.section.path.clone();
    let display = if path.contains('/') {
        path.clone()
    } else {
        format!("./{path}")
    };
    let toggle_path = path.clone();
    let toggle = inspector.clone();
    let open_path = path.clone();
    let mut counts = div()
        .flex_shrink_0()
        .flex()
        .gap(px_len(Spacing::XS))
        .fx_role(TextRole::MonoSmall);
    if header.section.additions > 0 {
        counts = counts.child(
            div()
                .text_color(Colors::diff_added_fg(theme))
                .child(SharedString::from(format!("+{}", header.section.additions))),
        );
    }
    if header.section.deletions > 0 {
        counts = counts.child(
            div()
                .text_color(Colors::diff_removed_fg(theme))
                .child(SharedString::from(format!("−{}", header.section.deletions))),
        );
    }
    div()
        .id(SharedString::from(format!("diff-header:{path}")))
        .debug_selector(|| "diff-header".into())
        .h(px(metrics.height))
        .w_full()
        .flex()
        .items_center()
        .gap(px_len(Spacing::XS))
        .pl(px(Spacing::SM + header.pinned))
        .pr(px_len(Spacing::SM))
        .bg(if header.selected {
            Colors::bg_selected(theme)
        } else {
            Colors::bg_elevated(theme)
        })
        .border_t(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(theme))
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            let _ = toggle.update(cx, |this, cx| this.toggle_section(&toggle_path, cx));
        })
        .child(
            if header.collapsed {
                Icon::CHEVRON_RIGHT
            } else {
                Icon::CHEVRON_DOWN
            }
            .render(IconSize::Small, Colors::fg_tertiary(theme), theme),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .fx_role(TextRole::CaptionMedium)
                .text_color(if header.selected {
                    Colors::fg(theme)
                } else {
                    Colors::fg_secondary(theme)
                })
                .child(SharedString::from(display)),
        )
        .child(counts)
        .child(glyph_button(
            SharedString::from(format!("diff-open:{path}")),
            Icon::ARROW_UP_RIGHT_SQUARE,
            "Open in Editor",
            move |_, _, cx| {
                let _ = inspector.update(cx, |this, cx| this.open_path(&open_path, cx));
            },
        ))
        .into_any_element()
}

/// The canvas row whose content is widest, which the list measures to size
/// its horizontal scroll.
fn widest_row(document: &DiffDocument, layout: &DiffRowLayout, collapsed: &[bool]) -> usize {
    document
        .sections
        .iter()
        .enumerate()
        .filter(|(index, section)| !collapsed[*index] && !section.lines.is_empty())
        .filter_map(|(index, section)| {
            let row = layout.header_row(index)? + 1 + section.widest_line;
            Some((section.widest_columns, row))
        })
        .max_by_key(|(columns, _)| *columns)
        .map_or(0, |(_, row)| row)
}

impl Inspector {
    /// Load the diff for the current comparison and snapshot, unless the one
    /// shown or loading already is that.
    pub(super) fn ensure_diff(&mut self, cx: &mut Context<Self>) {
        let monitor = self.monitor.read(cx);
        let generation = monitor.generation();
        let Some(snapshot) = monitor.snapshot().cloned() else {
            return;
        };
        let Some(repository) = snapshot.repository.as_ref() else {
            if self.diff.document.is_some() {
                self.diff.drop_document();
            }
            return;
        };
        let key = DiffKey {
            generation,
            mode: self.diff.mode,
            revision: repository.revision,
        };
        // A key that just failed is not retried until something changes or
        // Refresh is pressed; this runs on every render, so retrying here
        // would spawn a `git diff` per frame for as long as it keeps failing.
        if [self.diff.shown, self.diff.loading, self.diff.failed].contains(&Some(key)) {
            return;
        }
        if !repository.files.iter().any(|file| key.mode.includes(file)) {
            // Nothing to compare: no read, and nothing stale left behind.
            self.diff.document = Some(Arc::new(DiffDocument::default()));
            self.diff.shown = Some(key);
            self.diff.loading = None;
            self.diff.task = None;
            return;
        }
        self.diff.loading = Some(key);
        let root = snapshot.root.clone();
        let files = repository.files.clone();
        let has_commits = repository.has_commits;
        let job = repository::spawn_git(move || async move {
            repository::project_diff(&root, key.mode, files, has_commits).await
        });
        self.diff.task = Some(cx.spawn(async move |this, cx| {
            let result = repository::finish_git(job).await;
            let _ = this.update(cx, |this, cx| this.finish_diff(key, result, cx));
        }));
        cx.notify();
    }

    pub(super) fn finish_diff(
        &mut self,
        key: DiffKey,
        result: Result<DiffDocument, String>,
        cx: &mut Context<Self>,
    ) {
        if self.diff.loading != Some(key) {
            return;
        }
        self.diff.loading = None;
        self.diff.task = None;
        match result {
            Ok(document) => {
                let anchor = self.diff.anchor();
                if let Some(selected) = &self.diff.selected_path
                    && !document.sections.iter().any(|s| &s.path == selected)
                {
                    self.diff.selected_path = None;
                }
                self.diff.document = Some(Arc::new(document));
                self.diff.shown = Some(key);
                self.diff.error = None;
                if let Some(anchor) = anchor {
                    self.diff.restore(&anchor);
                }
            }
            // The previous document stays, labelled by the error above it.
            Err(error) => {
                self.diff.error = Some(error);
                self.diff.failed = Some(key);
            }
        }
        cx.notify();
    }

    /// Re-read the tree, and retry a diff that failed.
    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.diff.failed = None;
        self.monitor.update(cx, |monitor, cx| monitor.refresh(cx));
        self.ensure_diff(cx);
    }

    pub(super) fn set_mode(&mut self, mode: ComparisonMode, cx: &mut Context<Self>) {
        if self.diff.mode == mode {
            return;
        }
        self.diff.mode = mode;
        // Another comparison's lines must never sit under this one's label.
        self.diff.drop_document();
        self.ensure_diff(cx);
        cx.notify();
    }

    pub(super) fn set_layout(&mut self, layout: DiffLayout, cx: &mut Context<Self>) {
        if self.diff.layout == layout {
            return;
        }
        let anchor = self.diff.anchor();
        self.diff.layout = layout;
        if let Some(anchor) = anchor {
            self.diff.restore(&anchor);
        }
        cx.notify();
    }

    /// Show one file: expand it and scroll its header to the top.
    pub(super) fn select_path(&mut self, path: &str, cx: &mut Context<Self>) {
        self.diff.selected_path = Some(path.to_string());
        self.diff.disclosure.set_collapsed(false, path);
        if let Some(document) = &self.diff.document
            && let Some(index) = document.sections.iter().position(|s| s.path == path)
            && let Some(row) = self.diff.row_layout(document).header_row(index)
        {
            self.diff
                .scroll
                .scroll_to_item_strict(row, ScrollStrategy::Top);
        }
        cx.notify();
    }

    pub(super) fn toggle_section(&mut self, path: &str, cx: &mut Context<Self>) {
        self.diff.disclosure.toggle(path);
        self.diff.selected_path = Some(path.to_string());
        cx.notify();
    }

    pub(super) fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.diff.disclosure.set_all_collapsed(collapsed);
        match self.diff.selected_path.clone() {
            Some(path) if !collapsed => self.select_path(&path, cx),
            _ => self.diff.set_offset(0.0, 0.0),
        }
        cx.notify();
    }

    pub(super) fn open_path(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(root) = self
            .monitor
            .read(cx)
            .snapshot()
            .map(|snapshot| snapshot.root.clone())
        else {
            return;
        };
        let relative = std::path::Path::new(path);
        let inside = relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
        self.notice = if !inside {
            Some(format!(
                "{path} is outside the project and cannot be opened."
            ))
        } else {
            fx_core::services::editor::open_in_editor(&root.join(relative))
                .err()
                .map(|error| format!("Cannot open {path}: {error}"))
        };
        cx.notify();
    }

    /// Everything below the summary: the comparison toolbar, the navigator,
    /// and the canvas.
    pub(super) fn diff_body(
        &self,
        repository: &GitRepositoryInfo,
        cx: &Context<Self>,
    ) -> AnyElement {
        let mode = self.diff.mode;
        let visible = repository
            .files
            .iter()
            .filter(|file| mode.includes(file))
            .count();
        let mut body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.diff_toolbar(visible, cx));
        if let Some(error) = &self.diff.error {
            body = body.child(note(error, true));
        }
        if visible == 0 {
            return body
                .child(inspector_empty(
                    Icon::CHECKMARK_CIRCLE_FILL,
                    mode.empty_title(),
                    mode.empty_hint(),
                ))
                .into_any_element();
        }
        let Some(document) = &self.diff.document else {
            return body
                .child(note(
                    if self.diff.loading.is_some() {
                        "Loading diff…"
                    } else {
                        "Diff unavailable. Use Refresh to retry."
                    },
                    false,
                ))
                .into_any_element();
        };
        if let Some(notice) = &document.notice {
            body = body.child(note(notice, false));
        }
        if document.sections.is_empty() {
            return body
                .child(note(
                    "Git reports no textual diff for these changes.",
                    false,
                ))
                .into_any_element();
        }
        if self.diff.navigator_visible {
            body = body
                .child(self.navigator(document, cx))
                .child(divider_with_axis(DividerAxis::Horizontal));
        }
        body.child(self.canvas(document, cx)).into_any_element()
    }

    fn diff_toolbar(&self, visible: usize, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let modes: Vec<_> = ComparisonMode::ALL
            .into_iter()
            .map(|mode| SegmentedOption::new(mode, mode.title()))
            .collect();
        let layouts = [
            SegmentedOption::new(DiffLayout::Inline, "Inline"),
            SegmentedOption::new(DiffLayout::Split, "Split"),
        ];
        let has_sections = self
            .diff
            .document
            .as_ref()
            .is_some_and(|document| !document.sections.is_empty());
        let refreshing = self.diff.loading.is_some() && self.diff.document.is_some();
        // One control for both directions keeps the row inside the narrowest
        // panel: it offers whichever change would do something.
        let all_collapsed = self.diff.document.as_ref().is_some_and(|document| {
            document
                .sections
                .iter()
                .all(|section| self.diff.disclosure.is_collapsed(&section.path))
        });
        let (disclosure_icon, disclosure_label) = if all_collapsed {
            (Icon::ARROW_UP_LEFT_ARROW_DOWN_RIGHT, "Expand all diffs")
        } else {
            (Icon::ARROW_DOWN_RIGHT_ARROW_UP_LEFT, "Collapse all diffs")
        };
        div()
            .debug_selector(|| "diff-toolbar".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XS))
            .px(px_len(Spacing::MD))
            .pb(px_len(Spacing::SM))
            .child(
                div()
                    .debug_selector(|| "diff-mode-picker".into())
                    .flex()
                    .child(segmented_control(
                        &modes,
                        &self.diff.mode,
                        cx.listener(|this, mode: &ComparisonMode, _, cx| this.set_mode(*mode, cx)),
                    )),
            )
            .child(
                div()
                    .flex()
                    // Larger text sizes wrap the layout picker rather than
                    // clipping it.
                    .flex_wrap()
                    .items_center()
                    .gap(px_len(Spacing::XXS))
                    .child(compact_action_toggle(
                        format!("Files {visible}"),
                        Some(Icon::SIDEBAR_LEFT),
                        self.diff.navigator_visible,
                        has_sections,
                        cx.listener(|this, _, _, cx| {
                            this.diff.navigator_visible = !this.diff.navigator_visible;
                            cx.notify();
                        }),
                    ))
                    .child(icon_button(
                        IconButtonProps::new(disclosure_icon, disclosure_label)
                            .small()
                            .ghost()
                            .enabled(has_sections),
                        cx.listener(move |this, _, _, cx| {
                            this.set_all_collapsed(!all_collapsed, cx)
                        }),
                    ))
                    .child(div().flex_1().min_w_0().flex().justify_center().children(
                        refreshing.then(|| {
                            div()
                                .debug_selector(|| "diff-refreshing".into())
                                .id("diff-refreshing")
                                .tooltip(|window, cx| {
                                    fx_design::components::tooltip(
                                        "Refreshing the diff",
                                        window,
                                        cx,
                                    )
                                })
                                .child(Icon::ARROW_CLOCKWISE.render(
                                    IconSize::Small,
                                    Colors::fg_tertiary(&theme),
                                    &theme,
                                ))
                        }),
                    ))
                    .child(
                        div()
                            .debug_selector(|| "diff-layout-picker".into())
                            .flex_shrink_0()
                            .child(segmented_control(
                                &layouts,
                                &self.diff.layout,
                                cx.listener(|this, layout: &DiffLayout, _, cx| {
                                    this.set_layout(*layout, cx)
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn navigator(&self, document: &Arc<DiffDocument>, cx: &Context<Self>) -> AnyElement {
        let count = document.sections.len();
        let row = Layout::SIDEBAR_ROW_HEIGHT;
        let height =
            (count as f32 * row + Spacing::XXS * 2.0).min(Layout::DIFF_NAVIGATOR_COMPACT_HEIGHT);
        let document = document.clone();
        let selected = self.diff.selected_path.clone();
        let inspector = cx.weak_entity();
        uniform_list("inspector.navigator", count, move |range, _, _| {
            let theme = global::current();
            range
                .map(|index| {
                    navigator_row(
                        &document.sections[index],
                        document.statuses.get(index).map_or("", String::as_str),
                        selected.as_deref() == Some(document.sections[index].path.as_str()),
                        &theme,
                        inspector.clone(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .debug_selector(|| "diff-navigator".into())
        .h(px(height))
        .flex_shrink_0()
        .px(px_len(Spacing::XS))
        .py(px_len(Spacing::XXS))
        .into_any_element()
    }

    fn canvas(&self, document: &Arc<DiffDocument>, cx: &Context<Self>) -> AnyElement {
        let theme = global::current();
        let split = self.diff.layout == DiffLayout::Split;
        let collapsed = self.diff.collapsed(document);
        let layout = self.diff.row_layout(document);
        let max_line = document
            .sections
            .iter()
            .map(|section| section.max_line_number)
            .max()
            .unwrap_or(0);
        let metrics = RowMetrics {
            height: row_height(&theme),
            numbers: number_width(&theme, max_line),
        };
        let measure = if split {
            0
        } else {
            widest_row(document, &layout, &collapsed)
        };
        let count = layout.count();
        let document = document.clone();
        let selected = self.diff.selected_path.clone();
        let inspector = cx.weak_entity();
        let scroll = self.diff.scroll.clone();
        uniform_list("inspector.diff", count, move |range, _, _| {
            let theme = global::current();
            let pinned = if split {
                0.0
            } else {
                (-f32::from(scroll.0.borrow().base_handle.offset().x)).max(0.0)
            };
            range
                .map(|row| {
                    let Some(location) = layout.location(row) else {
                        return div().h(px(metrics.height)).into_any_element();
                    };
                    let section = &document.sections[location.section];
                    match location.line {
                        None => header_row(
                            HeaderRow {
                                section,
                                collapsed: collapsed[location.section],
                                selected: selected.as_deref() == Some(section.path.as_str()),
                                pinned,
                            },
                            metrics,
                            &theme,
                            inspector.clone(),
                        ),
                        Some(line) if split => split_row(section, line, metrics, &theme),
                        Some(line) => inline_row(&section.lines[line], metrics, &theme),
                    }
                })
                .collect::<Vec<_>>()
        })
        .debug_selector(|| "diff-canvas".into())
        .track_scroll(self.diff.scroll.clone())
        .with_horizontal_sizing_behavior(if split {
            ListHorizontalSizingBehavior::FitList
        } else {
            ListHorizontalSizingBehavior::Unconstrained
        })
        .with_width_from_item(Some(measure))
        .flex_1()
        .min_h_0()
        .w_full()
        .bg(Colors::content_bg(&theme))
        .into_any_element()
    }
}

fn navigator_row(
    section: &DiffSection,
    status: &str,
    selected: bool,
    theme: &Theme,
    inspector: WeakEntity<Inspector>,
) -> AnyElement {
    let path = section.path.clone();
    let (filename, parent) = split_file_path(&path);
    let (filename, parent) = (filename.to_string(), parent.to_string());
    let select = inspector.clone();
    let select_path = path.clone();
    let open_path = path.clone();
    let tooltip_path = path.clone();
    let hover = Colors::bg_hover(theme);
    let mut counts = div()
        .flex_shrink_0()
        .flex()
        .gap(px_len(Spacing::XXS))
        .fx_role(TextRole::MonoSmall);
    if section.additions > 0 {
        counts = counts.child(
            div()
                .text_color(Colors::diff_added_fg(theme))
                .child(SharedString::from(format!("+{}", section.additions))),
        );
    }
    if section.deletions > 0 {
        counts = counts.child(
            div()
                .text_color(Colors::diff_removed_fg(theme))
                .child(SharedString::from(format!("−{}", section.deletions))),
        );
    }
    let mut row = div()
        .id(SharedString::from(format!("inspector-file:{path}")))
        .debug_selector(|| "inspector-file-row".into())
        .h(px(Layout::SIDEBAR_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap(px_len(Spacing::SM))
        .px(px_len(Spacing::XS))
        .rounded(px_len(fx_design::theme::Radii::XS))
        .cursor_pointer()
        .hover(move |style| style.fx_background(hover))
        .on_click(move |_, _, cx| {
            let _ = select.update(cx, |this, cx| this.select_path(&select_path, cx));
        })
        .tooltip(move |window, cx| fx_design::components::tooltip(tooltip_path.clone(), window, cx))
        .child(div().flex().flex_shrink_0().child(Icon::DOC.render(
            IconSize::Small,
            Colors::fg_tertiary(theme),
            theme,
        )))
        .child(
            div()
                .min_w_0()
                .max_w(relative(0.55))
                .truncate()
                .fx_caption()
                .text_color(Colors::fg(theme))
                .child(SharedString::from(filename)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .fx_caption()
                .text_color(Colors::fg_tertiary(theme))
                .child(SharedString::from(parent)),
        )
        .child(counts)
        .child(
            div()
                .flex_shrink_0()
                .min_w(px_len(Spacing::MD))
                .fx_role(TextRole::MonoSmall)
                .text_color(status_ink(status, theme))
                .child(SharedString::from(status.to_string())),
        )
        .child(glyph_button(
            SharedString::from(format!("navigator-open:{path}")),
            Icon::ARROW_UP_RIGHT_SQUARE,
            "Open in Editor",
            move |_, _, cx| {
                let _ = inspector.update(cx, |this, cx| this.open_path(&open_path, cx));
            },
        ));
    if selected {
        row = row.bg(Colors::bg_selected(theme));
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::git::diff::parse_section;

    fn section() -> DiffSection {
        parse_section(
            Some("a.rs".into()),
            ["@@ -1,3 +1,2 @@", " keep", "-old", "-gone", "+new"],
        )
        .unwrap()
    }

    #[test]
    fn inline_lines_and_split_rows_map_onto_each_other() {
        let section = section();
        // Split rows: hunk, keep, (old,new), (gone,-).
        assert_eq!(section.split_rows.len(), 4);
        assert_eq!(inline_line_of(&section, 2, DiffLayout::Split), 2);
        assert_eq!(inline_line_of(&section, 3, DiffLayout::Split), 3);
        assert_eq!(
            row_of_inline_line(&section, 4, DiffLayout::Split),
            2,
            "+new pairs with -old"
        );
        assert_eq!(row_of_inline_line(&section, 3, DiffLayout::Split), 3);
        assert_eq!(row_of_inline_line(&section, 99, DiffLayout::Inline), 4);
    }

    #[test]
    fn display_text_expands_tabs_and_stays_bounded() {
        assert_eq!(visible_text("+\tx").as_ref(), "+   x");
        assert_eq!(visible_text("").as_ref(), " ");
        let long = visible_text(&"y".repeat(MAX_DISPLAY_CHARACTERS * 2));
        assert_eq!(long.chars().count(), MAX_DISPLAY_CHARACTERS + 2);
    }

    #[test]
    fn a_conflicts_status_letter_clears_three_to_one() {
        for dark in [false, true] {
            let theme = Theme {
                appearance_mode: if dark {
                    fx_design::theme::AppearanceMode::Dark
                } else {
                    fx_design::theme::AppearanceMode::Light
                },
                ..Default::default()
            };
            for background in [Colors::content_bg(&theme), Colors::bg_elevated(&theme)] {
                let ratio = fx_design::theme::contrast(status_ink("UU", &theme), background);
                assert!(ratio >= 3.0, "dark={dark}: {ratio}:1");
            }
        }
    }

    #[test]
    fn every_line_kind_is_legible_on_its_fill() {
        for dark in [false, true] {
            let theme = Theme {
                appearance_mode: if dark {
                    fx_design::theme::AppearanceMode::Dark
                } else {
                    fx_design::theme::AppearanceMode::Light
                },
                ..Default::default()
            };
            for kind in [
                DiffLineKind::Meta,
                DiffLineKind::Hunk,
                DiffLineKind::Context,
                DiffLineKind::Addition,
                DiffLineKind::Deletion,
            ] {
                let (fg, bg) = line_palette(kind, &theme);
                let fill = fx_design::theme::composite(bg, Colors::content_bg(&theme));
                let ratio = fx_design::theme::contrast(fg, fill);
                assert!(ratio >= 3.0, "{kind:?} in dark={dark}: {ratio}:1");
            }
        }
    }
}
