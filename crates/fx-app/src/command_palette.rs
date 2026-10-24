//! ⌘K: every command, thread, and project, one search away.
//!
//! Ported from `CommandPaletteView` in `MainLayout.swift`. Two parts, kept
//! apart so the rules are tested without a window:
//!
//! - **What is offered** — [`commands`] reads the state and lists what can be
//!   done right now. A command that cannot run is left out rather than shown
//!   disabled, as in the original: Stop appears only during a turn, Reset only
//!   outside one.
//! - **What matches** — [`rank`] scores a query against each command's title,
//!   keywords, and subtitle, and orders the matches.
//!
//! The view holds its highlight in a [`MenuModel`], so it moves, wraps, and
//! follows the pointer exactly as every other menu in the app does.
//!
//! A command runs by dispatching the same action its shortcut or menu item
//! dispatches, so there is one implementation of each, and the palette closes
//! first: a command that opens a sheet must not find the palette still open and
//! refuse, the way every modal refuses to open over another.

use std::cmp::Reverse;

use fx_design::components::{
    MenuItem, MenuModel, MenuSection, PaletteRow, empty_state, palette_panel, palette_row,
    shortcut_chip,
};
use fx_design::icon::Icon;
use fx_design::styled::px_len;
use fx_design::theme::{Colors, IconSize, Layout, Spacing, global};
use gpui::prelude::*;
use gpui::{
    Action, App, Context, Entity, FocusHandle, MouseButton, Render, ScrollHandle, SharedString,
    Subscription, Window, div,
};
use uuid::Uuid;

use crate::commands::{self, NUMBERED_THREADS};
use crate::state::AppState;
use crate::text_field::TextField;
use crate::workspace_ui;

/// The one section the palette's model holds.
const RESULTS: &str = "results";

/// What running a command does.
pub enum PaletteTarget {
    /// Dispatch an app action, exactly as its shortcut or menu item would.
    Action(Box<dyn Action>),
    /// Open a thread, in whichever project it belongs to.
    Thread(Uuid),
    /// Switch to a project.
    Project(Uuid),
}

impl Clone for PaletteTarget {
    fn clone(&self) -> Self {
        match self {
            Self::Action(action) => Self::Action(action.boxed_clone()),
            Self::Thread(id) => Self::Thread(*id),
            Self::Project(id) => Self::Project(*id),
        }
    }
}

/// One row the palette can offer.
#[derive(Clone)]
pub struct PaletteCommand {
    /// Unique in one listing; the highlight is keyed by it.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub icon: Icon,
    /// More words it answers to, lowercase.
    pub keywords: Vec<String>,
    /// The keys that run it, as symbols, when it has any.
    pub shortcut: Option<String>,
    pub target: PaletteTarget,
}

impl PaletteCommand {
    fn action(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        icon: Icon,
        keywords: &[&str],
        action: impl Action,
    ) -> Self {
        let shortcut = commands::shortcut_for(&action);
        Self {
            id: format!("action:{}", action.name()),
            title: title.into(),
            subtitle: subtitle.into(),
            icon,
            keywords: keywords.iter().map(|word| word.to_string()).collect(),
            shortcut,
            target: PaletteTarget::Action(Box::new(action)),
        }
    }
}

/// Everything the palette offers for the state as it stands, in display order:
/// commands first, then threads, then projects.
pub fn commands(state: &AppState) -> Vec<PaletteCommand> {
    use crate::commands::{
        ArchiveThread, DeleteThread, RenameThread, ResetConversation, ShowArchivedChats,
    };
    let mut out = vec![
        PaletteCommand::action(
            "Open project…",
            "Choose a folder and add it to the sidebar",
            Icon::FOLDER_BADGE_PLUS,
            &["add", "repo", "project", "folder", "open"],
            crate::NewProject,
        ),
        PaletteCommand::action(
            if state.sidebar_visible {
                "Hide sidebar"
            } else {
                "Show sidebar"
            },
            "Toggle the project sidebar",
            Icon::SIDEBAR_LEFT,
            &["sidebar", "navigation", "left", "toggle"],
            crate::ToggleSidebar,
        ),
        PaletteCommand::action(
            if state.snapshot.right_panel_visible {
                "Hide changes"
            } else {
                "Show changes"
            },
            "Toggle the changes panel",
            Icon::SIDEBAR_RIGHT,
            &[
                "git",
                "changes",
                "diff",
                "inspector",
                "right panel",
                "toggle",
            ],
            crate::ToggleInspector,
        ),
        PaletteCommand::action(
            "Settings",
            "Open the settings panel",
            Icon::GEARSHAPE,
            &["settings", "preferences", "config", "shortcuts"],
            crate::ShowSettings,
        ),
    ];

    let project = state.active_project();
    if let Some(project) = project {
        out.push(PaletteCommand::action(
            "New thread",
            format!("Start a thread in {}", project.name),
            Icon::PLUS_CIRCLE,
            &["thread", "conversation", "create", "new"],
            crate::NewThread,
        ));
    }
    if let Some(agent) = state.active_agent() {
        if state.is_thread_busy(agent.id) {
            out.push(PaletteCommand::action(
                "Stop current run",
                "Interrupt the running turn",
                Icon::STOP_CIRCLE_FILL,
                &["stop", "cancel", "interrupt"],
                crate::StopTurn,
            ));
        } else {
            out.push(PaletteCommand::action(
                "Reset conversation",
                "Clear messages and start a new provider session",
                Icon::ARROW_COUNTERCLOCKWISE,
                &["reset", "clear", "conversation", "session"],
                ResetConversation,
            ));
            out.push(PaletteCommand::action(
                "Rename thread…",
                "Give this thread a new name",
                Icon::PENCIL,
                &["thread", "rename", "title", "name"],
                RenameThread,
            ));
            out.push(PaletteCommand::action(
                "Archive thread",
                "Hide this thread; restore it from archived chats",
                Icon::ARCHIVEBOX,
                &["thread", "archive", "hide"],
                ArchiveThread,
            ));
            out.push(PaletteCommand::action(
                "Delete thread…",
                "Remove this thread and its saved conversation from FlowX",
                Icon::TRASH,
                &["thread", "delete", "remove", "trash"],
                DeleteThread,
            ));
        }
    }
    if state
        .snapshot
        .projects
        .iter()
        .any(|record| !record.archived_agent_ids.is_empty())
    {
        out.push(PaletteCommand::action(
            "Archived chats…",
            "Find and restore archived threads",
            Icon::ARCHIVEBOX,
            &["archived", "restore", "unarchive", "thread"],
            ShowArchivedChats,
        ));
    }

    // Threads: the active project's first, in sidebar order, so the ones with a
    // number key lead; then every other project's.
    let active_id = project.map(|project| project.id);
    let mut records: Vec<_> = state.snapshot.projects.iter().collect();
    records.sort_by_key(|record| record.project.as_ref().map(|p| p.id) != active_id);
    for record in records {
        let Some(project) = &record.project else {
            continue;
        };
        let active = Some(project.id) == active_id;
        let visible = project.agent_order.iter().filter(|id| {
            !record.archived_agent_ids.contains(id) && !record.missing_native_agent_ids.contains(id)
        });
        for (index, id) in visible.enumerate() {
            let Some(agent) = record.agents.iter().find(|agent| agent.id == *id) else {
                continue;
            };
            let title = if agent.title.is_empty() {
                "Untitled thread"
            } else {
                agent.title.as_str()
            };
            out.push(PaletteCommand {
                id: format!("thread:{id}"),
                title: format!("Open {title}"),
                subtitle: format!("Thread in {}", project.name),
                icon: Icon::BUBBLE_LEFT_AND_BUBBLE_RIGHT,
                keywords: vec![
                    "switch".into(),
                    "thread".into(),
                    title.to_lowercase(),
                    project.name.to_lowercase(),
                ],
                shortcut: (active && index < NUMBERED_THREADS)
                    .then(|| commands::shortcut_for(&commands::OpenThread { index }))
                    .flatten(),
                target: PaletteTarget::Thread(*id),
            });
        }
    }

    for record in &state.snapshot.projects {
        let Some(project) = &record.project else {
            continue;
        };
        if Some(project.id) == active_id {
            continue;
        }
        out.push(PaletteCommand {
            id: format!("project:{}", project.id),
            title: format!("Switch to {}", project.name),
            subtitle: project.root_path.clone(),
            icon: Icon::FOLDER,
            keywords: vec![
                "project".into(),
                "switch".into(),
                project.name.to_lowercase(),
            ],
            shortcut: None,
            target: PaletteTarget::Project(project.id),
        });
    }
    out
}

// ── Matching ───────────────────────────────────────────────────────────────

/// The indices of `commands` that match `query`, best first.
///
/// Every word of the query has to match somewhere — the title, a keyword, or
/// the subtitle — so "thread new" finds "New thread". A title match outranks a
/// keyword match, which outranks a subtitle match; ties keep display order, so
/// an empty query lists everything as [`commands`] ordered it.
pub fn rank(commands: &[PaletteCommand], query: &str) -> Vec<usize> {
    let words: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|word| word.chars().flat_map(char::to_lowercase).collect())
        .collect();
    let mut scored: Vec<(usize, u32)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            let mut total = 0;
            for word in &words {
                let title = fuzzy_score(word, &command.title).map(|score| score * 3);
                let keyword = command
                    .keywords
                    .iter()
                    .filter_map(|keyword| fuzzy_score(word, keyword))
                    .max()
                    .map(|score| score * 2);
                let subtitle = fuzzy_score(word, &command.subtitle);
                total += title.max(keyword).max(subtitle)?;
            }
            Some((index, total))
        })
        .collect();
    scored.sort_by_key(|&(index, score)| (Reverse(score), index));
    scored.into_iter().map(|(index, _)| index).collect()
}

/// How well `word` (already lowercase) matches `text`, or `None` when it does
/// not.
///
/// A run of the text equal to the word scores highest, more so at the start of
/// a word and earlier in the text. Otherwise the word's characters must appear
/// in order — "rsc" finds "Reset conversation" — scoring for each one, and more
/// for each that follows the last match directly or starts a word. Any
/// in-order match scores below any contiguous one.
fn fuzzy_score(word: &[char], text: &str) -> Option<u32> {
    const CONTIGUOUS: u32 = 1_000;
    if word.is_empty() {
        return Some(0);
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let starts_word = |at: usize| at == 0 || !text[at - 1].is_alphanumeric();

    if let Some(at) = text.windows(word.len()).position(|window| window == word) {
        let mut score = CONTIGUOUS + 200 - (at as u32).min(200);
        if starts_word(at) {
            score += 500;
        }
        return Some(score);
    }

    let mut next = 0;
    let mut score = 0;
    let mut previous = None;
    for (at, c) in text.iter().enumerate() {
        if next == word.len() {
            break;
        }
        if *c != word[next] {
            continue;
        }
        score += 1;
        if previous == Some(at.wrapping_sub(1)) {
            score += 4;
        }
        if starts_word(at) {
            score += 6;
        }
        previous = Some(at);
        next += 1;
    }
    (next == word.len()).then_some(score.min(CONTIGUOUS - 1))
}

// ── The view ───────────────────────────────────────────────────────────────

/// The palette, mounted over the window while it is open.
pub struct CommandPalette {
    state: Entity<AppState>,
    commands: Vec<PaletteCommand>,
    /// Indices into `commands`, best match first.
    results: Vec<usize>,
    /// The results as a menu, for the highlight and its keyboard rules.
    model: MenuModel,
    query: Entity<TextField>,
    /// What the results were ranked against, so a repaint that changed nothing
    /// does not reset the highlight.
    ranked_query: String,
    scroll: ScrollHandle,
    /// Where focus was before the palette took it, to give it back on Escape.
    previous_focus: Option<FocusHandle>,
    _query_changed: Subscription,
}

/// Open the palette, or close it when it is already open.
///
/// Refuses over another modal: a palette that opened over Settings could run a
/// command the sheet is there to prevent.
pub fn toggle(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    if let Some(open) = state.read(cx).palette.clone() {
        open.update(cx, |palette, cx| palette.dismiss(window, cx));
        return;
    }
    if workspace_ui::has_modal(&state, cx) {
        return;
    }
    let menu = state.read(cx).menu_layer.clone();
    menu.update(cx, |menu, cx| {
        if menu.dismiss().is_some() {
            cx.notify();
        }
    });
    let previous_focus = window.focused(cx);
    let commands = commands(state.read(cx));
    let palette = cx.new(|cx| {
        let query = cx.new(|cx| {
            TextField::new(cx)
                .bare()
                .placeholder("Search commands, threads, and projects")
                .max_len(200)
        });
        let query_changed = cx.observe(&query, |palette: &mut CommandPalette, _, cx| {
            palette.refilter(cx);
        });
        let mut palette = CommandPalette {
            state: state.clone(),
            commands,
            results: Vec::new(),
            model: MenuModel::default(),
            query,
            ranked_query: String::new(),
            scroll: ScrollHandle::new(),
            previous_focus,
            _query_changed: query_changed,
        };
        palette.rank_against(String::new());
        palette
    });
    let field = palette.read(cx).query.read(cx).focus_handle();
    state.update(cx, |state, cx| {
        state.palette = Some(palette);
        cx.notify();
    });
    field.focus(window);
}

impl CommandPalette {
    /// The query as typed.
    pub fn query(&self, cx: &App) -> String {
        self.query.read(cx).value().to_string()
    }

    /// The titles of the matching rows, best first.
    pub fn result_titles(&self) -> Vec<&str> {
        self.results
            .iter()
            .map(|&index| self.commands[index].title.as_str())
            .collect()
    }

    /// The highlighted row's title.
    pub fn highlighted_title(&self) -> Option<&str> {
        self.highlighted_index()
            .map(|index| self.commands[self.results[index]].title.as_str())
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query(cx);
        if query != self.ranked_query {
            self.rank_against(query);
            cx.notify();
        }
    }

    /// Rank against `query` and highlight the best match, as the Swift palette
    /// did on every change.
    fn rank_against(&mut self, query: String) {
        self.results = rank(&self.commands, &query);
        let items = self
            .results
            .iter()
            .map(|&index| {
                let command = &self.commands[index];
                MenuItem::new(command.id.clone(), command.title.clone())
            })
            .collect();
        self.model = MenuModel::new(vec![MenuSection::new(RESULTS, items)]);
        self.model.highlight_first();
        self.ranked_query = query;
        self.scroll.scroll_to_item(0);
    }

    /// Position of the highlighted row within `results`.
    fn highlighted_index(&self) -> Option<usize> {
        let key = self.model.highlighted()?;
        self.results
            .iter()
            .position(|&index| MenuModel::key_for(RESULTS, &self.commands[index].id) == key)
    }

    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.model.move_highlight(delta)
            && let Some(index) = self.highlighted_index()
        {
            // Keyboard movement has to keep the row in view; a hovered row
            // already is.
            self.scroll.scroll_to_item(index);
        }
        cx.notify();
    }

    /// Close without running anything, and give focus back to where it was.
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(cx);
        match &self.previous_focus {
            Some(focus) => focus.focus(window),
            None => window.blur(),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.palette = None;
            cx.notify();
        });
    }

    /// Run the highlighted row, if there is one.
    pub fn run_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.highlighted_index() {
            self.run(self.results[index], window, cx);
        }
    }

    fn run(&mut self, command: usize, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.commands[command].target.clone();
        self.dismiss(window, cx);
        match target {
            PaletteTarget::Action(action) => window.dispatch_action(action, cx),
            PaletteTarget::Thread(id) => self.state.update(cx, |state, cx| {
                state.open_thread(id);
                cx.notify();
            }),
            PaletteTarget::Project(id) => self.state.update(cx, |state, cx| {
                state.select_project(id);
                state.save();
                cx.notify();
            }),
        }
    }

    fn results_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = global::current();
        if self.results.is_empty() {
            return div()
                .w_full()
                .py(px_len(Spacing::XL))
                .child(empty_state(
                    Icon::SPARKLE_MAGNIFYINGGLASS.render(
                        IconSize::Large,
                        Colors::fg_tertiary(&theme),
                        &theme,
                    ),
                    "No matching commands",
                    "Try another word, or part of a thread's name.",
                ))
                .into_any_element();
        }
        let highlighted = self.model.highlighted().map(str::to_string);
        let this = cx.entity();
        let mut list = div()
            .id("command-palette-results")
            .debug_selector(|| "command-palette-results".into())
            .w_full()
            .max_h(px_len(Layout::COMMAND_PALETTE_MAX_LIST_HEIGHT))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XXS))
            .p(px_len(Spacing::SM));
        for &index in &self.results {
            let command = &self.commands[index];
            let key = MenuModel::key_for(RESULTS, &command.id);
            let for_hover = this.clone();
            let for_choose = this.clone();
            let hover_key = key.clone();
            list = list.child(palette_row(
                PaletteRow {
                    id: SharedString::from(key.clone()),
                    icon: command.icon,
                    title: command.title.clone().into(),
                    subtitle: command.subtitle.clone().into(),
                    shortcut: command.shortcut.clone().map(SharedString::from),
                    highlighted: highlighted.as_deref() == Some(key.as_str()),
                },
                move |_, cx| {
                    for_hover.update(cx, |palette, cx| {
                        if palette.model.highlighted() != Some(hover_key.as_str()) {
                            palette.model.set_highlight(&hover_key);
                            cx.notify();
                        }
                    });
                },
                move |window, cx| {
                    for_choose.update(cx, |palette, cx| palette.run(index, window, cx));
                },
            ));
        }
        list.into_any_element()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();
        let for_backdrop = this.clone();
        let for_escape = this.clone();

        let search = div()
            .w_full()
            .flex()
            .items_center()
            .gap(px_len(Spacing::SM))
            .px(px_len(Spacing::LG))
            .py(px_len(Spacing::MD))
            .child(Icon::MAGNIFYINGGLASS.render(
                IconSize::Medium,
                Colors::fg_tertiary(&theme),
                &theme,
            ))
            .child(div().flex_1().min_w_0().child(self.query.clone()))
            .child(
                div()
                    .id("command-palette-close")
                    .debug_selector(|| "command-palette-close".into())
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        for_escape.update(cx, |palette, cx| palette.dismiss(window, cx));
                    })
                    .child(shortcut_chip("esc")),
            );

        div()
            .id("command-palette")
            .debug_selector(|| "command-palette".into())
            .absolute()
            .size_full()
            .occlude()
            .bg(Colors::overlay(&theme))
            .flex()
            .flex_col()
            .items_center()
            .pt(px_len(Layout::COMMAND_PALETTE_TOP_INSET))
            .px(px_len(Spacing::XL))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                for_backdrop.update(cx, |palette, cx| palette.dismiss(window, cx));
            })
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            // Capture, so the keys are the palette's before the search field —
            // which would read Up and Down as caret moves — sees them.
            .capture_key_down(move |event, window, cx| {
                let key = event.keystroke.key.as_str();
                if !["up", "down", "enter", "escape"].contains(&key)
                    || event.keystroke.modifiers.modified()
                {
                    return;
                }
                cx.stop_propagation();
                this.update(cx, |palette, cx| match key {
                    "up" => palette.move_highlight(-1, cx),
                    "down" => palette.move_highlight(1, cx),
                    "enter" => palette.run_highlighted(window, cx),
                    _ => palette.dismiss(window, cx),
                });
            })
            .child(
                div()
                    .max_w_full()
                    // A press on the panel is not a press on the backdrop. The
                    // wrapper hugs the panel, so beside it still dismisses.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(palette_panel(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .child(search)
                            .child(fx_design::components::divider())
                            .child(self.results_list(cx)),
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(title: &str, subtitle: &str, keywords: &[&str]) -> PaletteCommand {
        PaletteCommand {
            id: title.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            icon: Icon::GEARSHAPE,
            keywords: keywords.iter().map(|word| word.to_string()).collect(),
            shortcut: None,
            target: PaletteTarget::Project(Uuid::nil()),
        }
    }

    fn titles<'a>(commands: &'a [PaletteCommand], query: &str) -> Vec<&'a str> {
        rank(commands, query)
            .into_iter()
            .map(|index| commands[index].title.as_str())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_everything_in_display_order() {
        let commands = [command("B", "", &[]), command("A", "", &[])];
        assert_eq!(titles(&commands, ""), ["B", "A"]);
        assert_eq!(titles(&commands, "   "), ["B", "A"]);
    }

    #[test]
    fn letters_in_order_match_and_a_contiguous_match_ranks_first() {
        let commands = [
            command("Reset conversation", "", &[]),
            command("Rename thread…", "", &[]),
            command("Settings", "", &[]),
        ];
        // "rsc" is in "ReSet Conversation" in order, and nowhere else.
        assert_eq!(titles(&commands, "rsc"), ["Reset conversation"]);
        // Both start with "re"; neither matches "xyz".
        assert_eq!(titles(&commands, "re").len(), 2);
        assert!(titles(&commands, "xyz").is_empty());
        // A substring beats a scattered match.
        let commands = [
            command("Toggle changes", "", &[]),
            command("Settings", "", &[]),
        ];
        assert_eq!(titles(&commands, "set")[0], "Settings");
    }

    #[test]
    fn every_word_must_match_somewhere_and_the_title_counts_most() {
        let commands = [
            command("New thread", "Start a thread in FlowX", &["create"]),
            command("Open First", "Thread in FlowX", &["switch"]),
            command("Settings", "Open the settings panel", &["preferences"]),
        ];
        assert_eq!(titles(&commands, "thread new"), ["New thread"]);
        assert_eq!(titles(&commands, "prefs"), ["Settings"], "keywords count");
        // A word in a title outranks the same word in a subtitle.
        assert_eq!(titles(&commands, "open")[0], "Open First");
        assert_eq!(titles(&commands, "open").len(), 2);
        // Case does not matter.
        assert_eq!(titles(&commands, "SETTINGS"), ["Settings"]);
    }

    #[test]
    fn a_match_at_the_start_of_a_word_beats_one_inside_a_word() {
        let word: Vec<char> = "art".chars().collect();
        let inside = fuzzy_score(&word, "Restart").unwrap();
        let start = fuzzy_score(&word, "Art board").unwrap();
        assert!(start > inside);
        assert_eq!(fuzzy_score(&word, "a r"), None, "the t is missing");
    }

    // ── Mounted ────────────────────────────────────────────────────────────

    use fx_core::{Agent, Project};
    use fx_store::{AppPaths, PersistedApp, PersistedProject, Store};
    use gpui::{TestAppContext, WindowHandle};

    struct Shell {
        test: TestAppContext,
        state: Entity<AppState>,
        window: WindowHandle<crate::FlowX>,
        threads: [Uuid; 2],
        _dir: tempfile::TempDir,
    }

    impl Shell {
        fn palette(&self) -> Option<Entity<CommandPalette>> {
            self.test.read(|cx| self.state.read(cx).palette.clone())
        }

        fn titles(&self) -> Vec<String> {
            let palette = self.palette().expect("the palette is open");
            self.test.read(|cx| {
                palette
                    .read(cx)
                    .result_titles()
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            })
        }

        fn highlighted(&self) -> Option<String> {
            let palette = self.palette()?;
            self.test
                .read(|cx| palette.read(cx).highlighted_title().map(str::to_string))
        }

        fn keys(&mut self, keys: &str) {
            self.test.simulate_keystrokes(self.window.into(), keys);
            self.test.refresh().unwrap();
        }

        fn type_text(&mut self, text: &str) {
            self.test.simulate_input(self.window.into(), text);
            self.test.refresh().unwrap();
        }
    }

    /// The whole shell over one project with two threads, the app's real key
    /// table installed, the first thread selected.
    fn shell() -> Shell {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let first = Agent::new("First");
        let second = Agent::new("Second");
        let mut project = Project::new(dir.path().to_string_lossy());
        project.agent_order = vec![first.id, second.id];
        let snapshot = PersistedApp {
            active_project_id: Some(project.id),
            active_agent_id: Some(first.id),
            projects: vec![PersistedProject {
                project: Some(project),
                agents: vec![first.clone(), second.clone()],
                is_expanded: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        Store::new(paths.clone()).save_app(&snapshot).unwrap();
        let mut test = TestAppContext::single();
        let state = test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths, cx));
            crate::AppHandle::install(state.clone(), cx);
            commands::install_key_bindings(cx);
            state
        });
        let view_state = state.clone();
        let window = test.add_window(move |_, _| crate::FlowX::new(view_state));
        test.refresh().unwrap();
        Shell {
            test,
            state,
            window,
            threads: [first.id, second.id],
            _dir: dir,
        }
    }

    #[test]
    fn cmd_k_opens_the_palette_and_return_runs_what_the_search_found() {
        let mut shell = shell();
        shell.keys("cmd-k");
        assert!(shell.palette().is_some());
        // Everything is offered before anything is typed, best first.
        assert_eq!(shell.highlighted().as_deref(), Some("Open project…"));
        assert!(shell.titles().contains(&"Open Second".to_string()));

        shell.type_text("second");
        assert_eq!(shell.titles()[0], "Open Second");
        assert_eq!(shell.highlighted().as_deref(), Some("Open Second"));
        shell.keys("enter");
        assert!(
            shell.palette().is_none(),
            "running a command closes the palette"
        );
        shell.test.read(|cx| {
            assert_eq!(
                shell.state.read(cx).snapshot.active_agent_id,
                Some(shell.threads[1])
            );
        });
    }

    #[test]
    fn arrows_move_the_highlight_and_a_command_runs_through_its_action() {
        let mut shell = shell();
        let sidebar = shell.test.read(|cx| shell.state.read(cx).sidebar_visible);
        shell.keys("cmd-k");
        shell.type_text("sidebar");
        // "Open project…" says "sidebar" too, but only in its subtitle.
        assert_eq!(shell.titles(), ["Hide sidebar", "Open project…"]);
        shell.keys("down");
        assert_eq!(shell.highlighted().as_deref(), Some("Open project…"));
        shell.keys("down");
        assert_eq!(
            shell.highlighted().as_deref(),
            Some("Hide sidebar"),
            "and wraps"
        );
        shell.keys("enter");
        shell.test.run_until_parked();
        shell.test.read(|cx| {
            assert_eq!(
                shell.state.read(cx).sidebar_visible,
                !sidebar,
                "⌘B's own handler ran"
            );
        });

        shell.keys("cmd-k");
        shell.keys("down down up");
        let titles = shell.titles();
        assert_eq!(shell.highlighted(), Some(titles[1].clone()));
    }

    #[test]
    fn escape_closes_the_palette_and_gives_focus_back() {
        let mut shell = shell();
        let composer = shell.test.update(|cx| {
            shell
                .state
                .update(cx, |state, cx| state.draft(shell.threads[0], cx))
        });
        let focused = |shell: &mut Shell| {
            let composer = composer.clone();
            shell
                .window
                .update(&mut shell.test, move |_, window, cx| {
                    composer.read(cx).focus_handle().is_focused(window)
                })
                .unwrap()
        };
        assert!(focused(&mut shell), "the composer starts with focus");
        shell.keys("cmd-k");
        assert!(!focused(&mut shell), "the search field takes it");
        shell.type_text("zzz");
        assert!(shell.titles().is_empty());
        shell.keys("enter");
        assert!(
            shell.palette().is_some(),
            "Return with no match does nothing"
        );
        shell.keys("escape");
        assert!(shell.palette().is_none());
        assert!(focused(&mut shell), "and the composer has it back");
        // The keys the palette held are free again.
        shell.keys("cmd-k cmd-k");
        assert!(shell.palette().is_none(), "⌘K a second time closes it");
    }

    #[test]
    fn the_palette_offers_stop_during_a_turn_and_reset_outside_one() {
        let shell = shell();
        let titles = |shell: &Shell| -> Vec<String> {
            shell.test.read(|cx| {
                commands(shell.state.read(cx))
                    .into_iter()
                    .map(|command| command.title)
                    .collect()
            })
        };
        let idle = titles(&shell);
        assert!(idle.contains(&"Reset conversation".to_string()));
        assert!(!idle.contains(&"Stop current run".to_string()));
        shell.test.update(|cx| {
            let conversation = shell
                .state
                .update(cx, |state, cx| state.active_conversation(cx))
                .unwrap();
            conversation
                .lock()
                .unwrap()
                .set_phase(fx_agent::conversation::Phase::Running);
        });
        let busy = titles(&shell);
        assert!(busy.contains(&"Stop current run".to_string()));
        assert!(!busy.contains(&"Reset conversation".to_string()));
        assert!(!busy.contains(&"Delete thread…".to_string()));
        // Threads in the active project carry their number key.
        let commands = shell.test.read(|cx| commands(shell.state.read(cx)));
        let second = commands
            .iter()
            .find(|command| command.title == "Open Second")
            .unwrap();
        assert_eq!(second.shortcut.as_deref(), Some("⌘2"));
    }
}
