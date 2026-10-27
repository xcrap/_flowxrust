//! The settings sheet: which CLIs FlowX can reach, and how it looks.
//!
//! Two things were unreachable before this: whether the user's provider CLI is
//! actually usable, and any way to change the theme. The menu item that would
//! have opened a place for both is disabled in [`crate::app_menu`].
//!
//! # Two halves, deliberately separated
//!
//! [`SettingsDraft`] is the edits as a value, and [`health_rows`] is the
//! presenter over what discovery found. Neither needs a window, so every rule
//! below is asserted without one.
//!
//! [`SettingsSheet`] owns only what a view needs: the draft, the rows, a focus
//! handle, and three callbacks. Appearance choices preview immediately across
//! the window. The baseline stays separate from that preview: Reset and Cancel
//! restore it, while Done asks the caller to persist the draft.

use std::path::{Path, PathBuf};

use fx_core::services::runtime::{BinaryHealth, BinarySpec, expand_glob};
use fx_design::components::segmented::{SegmentedOption, segmented_control};
use fx_design::components::{
    BadgeTone, ButtonStyle, DividerAxis, IconButtonProps, badge, breadcrumb, button,
    button_with_icon, divider, divider_with_axis, icon_button, navigation_button, settings_group,
    settings_nav_label, settings_row, settings_row_custom, settings_row_dense, settings_search,
    settings_section, settings_section_with_action, tooltip,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{
    AccentColor, AppearanceMode, BaseTone, BorderWidth, Colors, Layout, Radii, Shadow, Spacing,
    TextRole, TextSizePreset, Theme, global,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, Render, SharedString,
    Subscription, Window, div, px,
};

use crate::text_field::TextField;

// ── The draft ──────────────────────────────────────────────────────────────

/// The design settings as they are being edited.
///
/// Four fields, matching the four things `fx-design`'s [`Theme`] resolves from
/// that a person chooses. Deliberately not a `Theme`: a theme also carries
/// [`Theme::system_appearance`], which belongs to the OS observer and must
/// survive an apply untouched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SettingsDraft {
    pub base_tone: BaseTone,
    pub appearance: AppearanceMode,
    pub accent: AccentColor,
    pub text_size: TextSizePreset,
}

impl SettingsDraft {
    /// The settings the window is currently showing.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            base_tone: theme.base_tone,
            appearance: theme.appearance_mode,
            accent: theme.accent_color,
            text_size: theme.text_size,
        }
    }

    /// The theme these edits describe, applied over `current`.
    ///
    /// `current` is the theme in effect rather than a fresh `Theme::default`,
    /// because the draft owns four fields and a `Theme` has five. Building a
    /// default and overwriting four fields would reset the fifth — the resolved
    /// OS appearance — to `true` and silently repaint a light-mode window dark.
    pub fn apply_to(self, current: &Theme) -> Theme {
        current
            .with_tone(self.base_tone)
            .with_appearance(self.appearance)
            .with_accent(self.accent)
            .with_text_size(self.text_size)
    }

    /// Whether these edits differ from `current` in any way.
    ///
    /// The comparison is over the draft rather than over the resolved theme, so
    /// it covers exactly the settings the draft owns. Every field is named
    /// explicitly instead of derived, because a comparison over a subset is how
    /// a setting gets silently dropped — and a dropped setting is a control the
    /// reader moves that then does nothing when they press Done.
    pub fn is_dirty_against(&self, current: &SettingsDraft) -> bool {
        self.base_tone != current.base_tone
            || self.appearance != current.appearance
            || self.accent != current.accent
            || self.text_size != current.text_size
    }
}

// ── The choice tables ──────────────────────────────────────────────────────

/// One exclusive choice, as the sheet's segmented controls need it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Choice<T> {
    pub value: T,
    /// From the design system's own `label()`, never written out here.
    pub label: &'static str,
}

/// The base tones, in the design system's declared order.
///
/// Built from `BaseTone::ALL` rather than listed, so a tone added upstream
/// cannot be silently left out of the sheet.
pub fn tone_choices() -> Vec<Choice<BaseTone>> {
    BaseTone::ALL
        .iter()
        .map(|&tone| Choice {
            value: tone,
            label: tone.label(),
        })
        .collect()
}

pub fn appearance_choices() -> Vec<Choice<AppearanceMode>> {
    AppearanceMode::ALL
        .iter()
        .map(|&mode| Choice {
            value: mode,
            label: mode.label(),
        })
        .collect()
}

pub fn accent_choices() -> Vec<Choice<AccentColor>> {
    AccentColor::ALL
        .iter()
        .map(|&accent| Choice {
            value: accent,
            label: accent.label(),
        })
        .collect()
}

pub fn text_size_choices() -> Vec<Choice<TextSizePreset>> {
    TextSizePreset::ALL
        .iter()
        .map(|&preset| Choice {
            value: preset,
            label: preset.label(),
        })
        .collect()
}

/// The choice a label names, or `None` when the label names nothing.
///
/// The only path by which a stored choice becomes a live one, so it refuses
/// rather than defaulting: a tone `fx-design` has since dropped must not quietly
/// become whatever the app happens to default to.
pub fn tone_from_label(label: &str) -> Option<BaseTone> {
    choice_by_label(&tone_choices(), label)
}

pub fn appearance_from_label(label: &str) -> Option<AppearanceMode> {
    choice_by_label(&appearance_choices(), label)
}

pub fn accent_from_label(label: &str) -> Option<AccentColor> {
    choice_by_label(&accent_choices(), label)
}

pub fn text_size_from_label(label: &str) -> Option<TextSizePreset> {
    choice_by_label(&text_size_choices(), label)
}

/// Exact match only, case included: labels are what the sheet shows, so a
/// case-folded lookup would accept a string no control ever renders.
fn choice_by_label<T: Copy + PartialEq>(choices: &[Choice<T>], label: &str) -> Option<T> {
    choices
        .iter()
        .find(|choice| choice.label == label)
        .map(|choice| choice.value)
}

// ── Provider health ────────────────────────────────────────────────────────

/// How serious a provider's state is.
///
/// Ordered by how much the reader can do about it, not by how FlowX feels about
/// it. The distinction between [`RowTone::Idle`] and [`RowTone::Bad`] is the
/// whole point: a red row for "you have not installed codex" trains people to
/// ignore red, and then the one red row that matters does not land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTone {
    /// Found and runnable.
    Ok,
    /// Found, but the reader has to clear something before it will run.
    Warn,
    /// Something is in the way and FlowX will not work until it moves.
    Bad,
    /// Nothing to report, or nothing to do.
    Idle,
}

impl RowTone {
    pub const ALL: [Self; 4] = [Self::Ok, Self::Warn, Self::Bad, Self::Idle];
}

/// The badge tone a row is drawn with.
///
/// `Idle` is neutral rather than a success wash: "not installed" is not a
/// result, and giving it a green pill would say that it is.
pub fn badge_tone(tone: RowTone) -> BadgeTone {
    match tone {
        RowTone::Ok => BadgeTone::Success,
        RowTone::Warn => BadgeTone::Warning,
        RowTone::Bad => BadgeTone::Error,
        RowTone::Idle => BadgeTone::Neutral,
    }
}

/// One provider, paired with what discovery found for it.
///
/// A pair rather than a bare [`BinaryHealth`] because two of the rules below are
/// about the *spec*: which install hint to quote, and whether a directory is
/// sitting where the binary should be. Neither is knowable from the health alone
/// — `BinaryHealth::NotFound` carries no path.
#[derive(Debug, Clone, Copy)]
pub struct ProviderReport<'a> {
    pub spec: &'a BinarySpec,
    pub health: &'a BinaryHealth,
}

impl<'a> ProviderReport<'a> {
    pub fn new(spec: &'a BinarySpec, health: &'a BinaryHealth) -> Self {
        Self { spec, health }
    }
}

/// What the sheet shows for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRow {
    /// The spec's id, so a caller can match a row to its provider.
    pub id: String,
    /// The name to put on the row.
    pub title: String,
    pub tone: RowTone,
    /// Short label for the badge beside the name.
    pub badge: String,
    /// One line saying what to do, or what is there.
    pub detail: String,
    /// The resolved path, when discovery reported one.
    pub path: Option<String>,
}

impl HealthRow {
    /// Whether the reader can act on [`Self::path`] themselves.
    ///
    /// A method rather than a stored flag: it is derived from the path, and a
    /// second copy of a derived answer is a second thing to be wrong.
    pub fn path_is_actionable(&self) -> bool {
        self.path.as_deref().is_some_and(is_actionable_path)
    }
}

/// Whether a path is one the reader can be sent to.
///
/// `false` inside an app bundle. The quarantine instruction is "open it in
/// Finder and choose Open", and Finder does not offer that for a file inside a
/// signed bundle — so sending someone there is an instruction with no action at
/// the end of it. The honest answer for that binary is to reinstall the app.
///
/// `false` for a relative path too: discovery reports resolved absolute paths,
/// and a relative one is either a bug or something the process will resolve
/// against a working directory the reader never chose.
pub fn is_actionable_path(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && !path
            .components()
            .any(|component| component.as_os_str().to_string_lossy().ends_with(".app"))
}

/// One row per reported provider, in the order they were reported.
///
/// Order is preserved rather than sorted, and nothing is de-duplicated: the rows
/// are rendered directly, so a stable order is the contract. Two reports of the
/// same binary are two facts and both are shown — folding them would hide the
/// one that matters if the first was the stale reading.
pub fn health_rows(reports: &[ProviderReport<'_>]) -> Vec<HealthRow> {
    reports
        .iter()
        .map(|report| health_row(report.spec, report.health))
        .collect()
}

fn health_row(spec: &BinarySpec, health: &BinaryHealth) -> HealthRow {
    let row = |tone: RowTone, badge: &str, detail: String, path: Option<String>| HealthRow {
        id: spec.id.clone(),
        title: spec.display_name.clone(),
        tone,
        badge: badge.to_string(),
        detail,
        path,
    };
    let name = &spec.display_name;

    match health {
        // Still being looked up: nothing is known and nothing is wrong. Given a
        // tone it would be a wash of colour for a question.
        BinaryHealth::Checking => row(
            RowTone::Idle,
            "Checking",
            format!("Checking for {name}…"),
            None,
        ),
        BinaryHealth::Available {
            path,
            version: Some(version),
        } => row(
            RowTone::Ok,
            "Ready",
            format!("{name} {version}"),
            Some(path.clone()),
        ),
        BinaryHealth::Available { path, .. } => row(
            RowTone::Ok,
            "Ready",
            format!("{name} is installed."),
            Some(path.clone()),
        ),
        // Warn, not bad: the binary is installed and one right-click clears it.
        // The wording is `BinaryHealth::explanation`'s, because it is the
        // app's policy about what to tell someone about quarantine, and a
        // second wording beside it would be one more to keep in step.
        BinaryHealth::Quarantined { path } => {
            let detail = if is_actionable_path(path) {
                format!(
                    "{name} at {path} has not been approved. \
                     Open it in Finder and choose Open, then restart FlowX."
                )
            } else {
                format!(
                    "{name} has not been approved, and it is inside an app bundle \
                     where approving it by hand is not offered. \
                     Reinstall that app to clear it."
                )
            };
            row(RowTone::Warn, "Not approved", detail, Some(path.clone()))
        }
        BinaryHealth::NotFound => match shadowing_directory(spec) {
            // Bad, not idle: something *is* there and it is the wrong kind of
            // thing, and the fix is to move it. The install hint is deliberately
            // left out — the reader does not need to install anything, and
            // telling them to is what makes this look like "not installed".
            Some(directory) => {
                let expected = Path::new(&directory)
                    .file_name()
                    .map(|file| file.to_string_lossy().into_owned())
                    .unwrap_or_else(|| spec.id.clone());
                let detail = format!(
                    "{name} is a folder, not a program, and it is all that is at {directory}. \
                     Remove or rename it so the real {expected} can be found."
                );
                row(RowTone::Bad, "Not a program", detail, Some(directory))
            }
            // Idle, not bad: nothing is installed and the sheet cannot install
            // it. A red row here is a permanent state, and red that is always on
            // is red that stops being read.
            None => row(RowTone::Idle, "Not installed", not_found_detail(spec), None),
        },
    }
}

/// The install line, mirroring `BinaryHealth::explanation`'s wording for
/// `NotFound` so the two cannot drift.
fn not_found_detail(spec: &BinarySpec) -> String {
    match &spec.install_hint {
        Some(hint) => format!("{} was not found. {hint}", spec.display_name),
        None => format!("{} was not found.", spec.display_name),
    }
}

/// The first of a spec's own paths that is a folder rather than a program.
///
/// Only consulted once discovery has already reported nothing. `fx-core` rejects
/// a directory in `is_executable`, so a directory is reported as `NotFound` and
/// is otherwise indistinguishable from nothing being installed — but the two
/// need different instructions, so the sheet asks the disk which one it was.
///
/// The spec's *own* paths, not the discovery's extra directories: those are the
/// paths the spec means, and they are the ones a versioned install names.
fn shadowing_directory(spec: &BinarySpec) -> Option<String> {
    spec.search_paths()
        .iter()
        .find_map(|pattern| {
            let candidates: Vec<PathBuf> = if pattern.contains('*') {
                expand_glob(pattern)
            } else {
                vec![PathBuf::from(pattern)]
            };
            candidates.into_iter().find(|candidate| {
                std::fs::metadata(candidate).is_ok_and(|metadata| metadata.is_dir())
            })
        })
        .map(|found| found.to_string_lossy().into_owned())
}

// ── The sheet ──────────────────────────────────────────────────────────────

/// Called with the draft, when the reader applies it.
type ApplyHandler = Box<dyn Fn(SettingsDraft, &mut Window, &mut App)>;
/// Called when the reader backs out without applying anything.
type DismissHandler = Box<dyn Fn(&mut Window, &mut App)>;
/// Called when the reader asks for the CLIs to be looked for again.
type RecheckHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// Element id of the container that owns the sheet's keys.
const SHEET_ID: &str = "settings.sheet";

/// A search names a setting; it is never a paragraph.
const SEARCH_MAX_LEN: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsPage {
    #[default]
    Appearance,
    ChatDefaults,
    Shortcuts,
    Providers,
    Archives,
}

impl SettingsPage {
    #[cfg(test)]
    const ALL: [Self; 5] = [
        Self::Appearance,
        Self::ChatDefaults,
        Self::Shortcuts,
        Self::Providers,
        Self::Archives,
    ];

    /// The navigation's runs, in order, each under its label.
    const GROUPS: [(Option<&'static str>, &'static [Self]); 3] = [
        (
            None,
            &[Self::Appearance, Self::ChatDefaults, Self::Shortcuts],
        ),
        (Some("Agents"), &[Self::Providers]),
        (Some("History"), &[Self::Archives]),
    ];

    /// Words a reader might search for that the label does not say: the
    /// settings on the page and the values they take.
    fn search_terms(self) -> &'static [&'static str] {
        match self {
            Self::Appearance => &[
                "theme",
                "light",
                "dark",
                "system",
                "mode",
                "base color",
                "tone",
                "slate",
                "zinc",
                "neutral",
                "stone",
                "accent",
                "violet",
                "blue",
                "emerald",
                "orange",
                "rose",
                "text size",
                "font",
            ],
            Self::ChatDefaults => &[
                "codex",
                "claude",
                "model",
                "reasoning",
                "effort",
                "access",
                "permissions",
                "supervised",
                "while responding",
                "steer",
                "queue",
                "command-return",
                "chat names",
                "titles",
                "automatic names",
            ],
            Self::Shortcuts => &["keys", "keyboard", "bindings", "hotkeys"],
            Self::Providers => &[
                "cli",
                "codex",
                "claude",
                "binary",
                "path",
                "installed",
                "quarantine",
                "check again",
                "runtime",
            ],
            Self::Archives => &["archive", "restore", "history"],
        }
    }

    /// Whether the page offers what `query` asks for. `query` is already
    /// trimmed and lowercased; an empty one matches every page.
    fn matches(self, query: &str) -> bool {
        query.is_empty()
            || self.label().to_lowercase().contains(query)
            || self.search_terms().iter().any(|term| term.contains(query))
            || (self == Self::Shortcuts
                && crate::commands::key_bindings()
                    .iter()
                    .any(|shortcut| shortcut.label.to_lowercase().contains(query)))
    }

    fn label(self) -> &'static str {
        match self {
            Self::ChatDefaults => "Chat defaults",
            Self::Appearance => "Appearance",
            Self::Providers => "Providers",
            Self::Shortcuts => "Shortcuts",
            Self::Archives => "Archived chats",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ChatDefaults => "chat-defaults",
            Self::Appearance => "appearance",
            Self::Providers => "providers",
            Self::Shortcuts => "shortcuts",
            Self::Archives => "archives",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::ChatDefaults => Icon::BUBBLE_LEFT_AND_BUBBLE_RIGHT,
            Self::Appearance => Icon::GEARSHAPE,
            Self::Providers => Icon::TERMINAL,
            Self::Shortcuts => Icon::KEYBOARD,
            Self::Archives => Icon::ARCHIVEBOX,
        }
    }
}

/// The settings sheet, mounted over the window by its caller.
///
/// The sheet owns a reversible appearance preview. The caller owns persistence
/// and the decision to unmount.
pub struct SettingsSheet {
    /// The settings as they were when the sheet opened.
    ///
    /// Captured rather than re-read from the theme on Reset, so a theme changed
    /// from outside — the appearance cycling in the title bar, say — cannot make
    /// Reset restore something the reader never saw.
    baseline: SettingsDraft,
    draft: SettingsDraft,
    rows: Vec<HealthRow>,
    focus: FocusHandle,
    on_apply: ApplyHandler,
    on_dismiss: DismissHandler,
    on_recheck: RecheckHandler,
    provider_defaults: Option<Entity<crate::provider_settings::ProviderSettings>>,
    provider_defaults_dirty: bool,
    provider_defaults_subscription: Option<Subscription>,
    title_settings: Option<Entity<crate::title_settings::TitleSettings>>,
    title_settings_dirty: bool,
    title_settings_subscription: Option<Subscription>,
    follow_up: Option<Entity<crate::follow_up_settings::FollowUpSettings>>,
    follow_up_dirty: bool,
    follow_up_subscription: Option<Subscription>,
    archives: Option<Entity<crate::archived_chats::ArchivedChats>>,
    page: SettingsPage,
    /// Narrows the navigation to the pages that offer what it names.
    search: Entity<TextField>,
    _search_subscription: Subscription,
    /// Set once either outcome has been reported.
    ///
    /// The caller unmounts on the first callback and reacts to it, not the
    /// second, so a second Return landing in the gap must not apply twice.
    settled: bool,
}

impl SettingsSheet {
    /// Build a sheet over the theme currently in effect.
    ///
    /// Takes `cx` because the focus handle is an app-level thing: without one,
    /// nothing holds focus and the key handler below is unreachable in practice
    /// however reachable it is in principle.
    pub fn new(
        cx: &mut App,
        on_apply: ApplyHandler,
        on_dismiss: DismissHandler,
        on_recheck: RecheckHandler,
    ) -> Entity<Self> {
        let draft = SettingsDraft::from_theme(&global::current());
        let mut focus = cx.focus_handle();
        // Reachable from the keyboard at all. Nothing here is a text field, so
        // nothing else would ever hold focus.
        focus.tab_stop = true;

        cx.new(|cx| {
            let search = cx.new(|cx| {
                TextField::new(cx)
                    .bare()
                    .placeholder("Search")
                    .max_len(SEARCH_MAX_LEN)
            });
            let search_subscription = cx.observe(&search, |_, _, cx| cx.notify());
            Self {
                baseline: draft,
                draft,
                rows: Vec::new(),
                focus,
                on_apply,
                on_dismiss,
                on_recheck,
                provider_defaults: None,
                provider_defaults_dirty: false,
                provider_defaults_subscription: None,
                title_settings: None,
                title_settings_dirty: false,
                title_settings_subscription: None,
                follow_up: None,
                follow_up_dirty: false,
                follow_up_subscription: None,
                archives: None,
                page: SettingsPage::default(),
                search,
                _search_subscription: search_subscription,
                settled: false,
            }
        })
    }

    /// The search as the pages are matched against it.
    fn search_query(&self, cx: &App) -> String {
        self.search.read(cx).value().trim().to_lowercase()
    }

    /// The pages the navigation shows, in its order.
    fn visible_pages(&self, cx: &App) -> Vec<SettingsPage> {
        let query = self.search_query(cx);
        SettingsPage::GROUPS
            .iter()
            .flat_map(|(_, pages)| pages.iter().copied())
            .filter(|page| self.page_available(*page) && page.matches(&query))
            .collect()
    }

    /// Return from the search to the first page it found, if any.
    fn open_first_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.visible_pages(cx).first().copied() {
            self.select_page(page, window, cx);
        }
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value("", cx));
    }

    /// Build a sheet and put focus on it.
    ///
    /// The usual entry point: the sheet is opened from a menu row rather than
    /// clicked into, so without this a reader has to click the backdrop before
    /// Escape works.
    pub fn open(
        window: &mut Window,
        cx: &mut App,
        on_apply: ApplyHandler,
        on_dismiss: DismissHandler,
        on_recheck: RecheckHandler,
    ) -> Entity<Self> {
        let sheet = Self::new(cx, on_apply, on_dismiss, on_recheck);
        sheet.read(cx).focus.focus(window);
        sheet
    }

    /// The handle the sheet's key handler is reached through.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub(crate) fn set_archives(&mut self, archives: Entity<crate::archived_chats::ArchivedChats>) {
        self.archives = Some(archives);
    }

    pub(crate) fn set_provider_defaults(
        &mut self,
        editor: Entity<crate::provider_settings::ProviderSettings>,
        cx: &mut Context<Self>,
    ) {
        self.provider_defaults_dirty = editor.read(cx).is_dirty();
        self.provider_defaults_subscription = Some(cx.observe(&editor, |sheet, editor, cx| {
            sheet.provider_defaults_dirty = editor.read(cx).is_dirty();
            cx.notify();
        }));
        self.provider_defaults = Some(editor);
        self.page = SettingsPage::ChatDefaults;
        cx.notify();
    }

    fn page_available(&self, page: SettingsPage) -> bool {
        match page {
            SettingsPage::ChatDefaults => {
                self.provider_defaults.is_some()
                    || self.title_settings.is_some()
                    || self.follow_up.is_some()
            }
            SettingsPage::Archives => self.archives.is_some(),
            _ => true,
        }
    }

    fn select_page(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) {
        if !self.page_available(page) {
            return;
        }
        self.dismiss_provider_menu(cx);
        self.page = page;
        if page == SettingsPage::Archives {
            if let Some(archives) = &self.archives {
                archives.read(cx).focus_search(window, cx);
            }
        } else {
            self.focus.focus(window);
        }
        cx.notify();
    }

    /// Show the archived chats, for the Threads menu's "Archived chats…".
    pub(crate) fn show_archives(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_page(SettingsPage::Archives, window, cx);
    }

    pub(crate) fn provider_defaults(
        &self,
    ) -> Option<Entity<crate::provider_settings::ProviderSettings>> {
        self.provider_defaults.clone()
    }

    pub(crate) fn set_title_settings(
        &mut self,
        editor: Entity<crate::title_settings::TitleSettings>,
        cx: &mut Context<Self>,
    ) {
        self.title_settings_dirty = editor.read(cx).is_dirty();
        self.title_settings_subscription = Some(cx.observe(&editor, |sheet, editor, cx| {
            sheet.title_settings_dirty = editor.read(cx).is_dirty();
            cx.notify();
        }));
        self.title_settings = Some(editor);
        self.page = SettingsPage::ChatDefaults;
        cx.notify();
    }

    pub(crate) fn title_settings(&self) -> Option<Entity<crate::title_settings::TitleSettings>> {
        self.title_settings.clone()
    }

    /// Show the "While responding" choice on the chat defaults page.
    pub(crate) fn set_follow_up_settings(
        &mut self,
        editor: Entity<crate::follow_up_settings::FollowUpSettings>,
        cx: &mut Context<Self>,
    ) {
        self.follow_up_dirty = editor.read(cx).is_dirty();
        self.follow_up_subscription = Some(cx.observe(&editor, |sheet, editor, cx| {
            sheet.follow_up_dirty = editor.read(cx).is_dirty();
            cx.notify();
        }));
        self.follow_up = Some(editor);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn follow_up_settings(
        &self,
    ) -> Option<Entity<crate::follow_up_settings::FollowUpSettings>> {
        self.follow_up.clone()
    }

    fn settings_menu_is_open(&self, cx: &App) -> bool {
        self.provider_defaults
            .as_ref()
            .is_some_and(|editor| editor.read(cx).menu_is_open(cx))
            || self
                .title_settings
                .as_ref()
                .is_some_and(|editor| editor.read(cx).menu_is_open(cx))
    }

    fn dismiss_provider_menu(&self, cx: &mut App) {
        if let Some(editor) = &self.provider_defaults {
            editor.update(cx, |editor, cx| editor.dismiss_menu(cx));
        }
        if let Some(editor) = &self.title_settings {
            editor.update(cx, |editor, cx| editor.dismiss_menu(cx));
        }
    }

    /// The settings as they stand.
    pub fn draft(&self) -> SettingsDraft {
        self.draft
    }

    /// The provider rows as they stand.
    pub fn rows(&self) -> &[HealthRow] {
        &self.rows
    }

    /// Whether applying would change anything.
    pub fn is_dirty(&self) -> bool {
        self.draft.is_dirty_against(&self.baseline)
            || self.provider_defaults_dirty
            || self.title_settings_dirty
            || self.follow_up_dirty
    }

    /// Whether Reset has anything to undo.
    pub fn can_reset(&self) -> bool {
        self.is_dirty()
    }

    /// Replace the provider rows.
    ///
    /// The caller owns the re-scan: discovery is async and may shell out to a
    /// login shell, so the sheet cannot run it from a click handler. It hands
    /// the rows back when the scan finishes.
    pub fn set_health(&mut self, cx: &mut Context<Self>, reports: &[ProviderReport<'_>]) {
        self.rows = health_rows(reports);
        cx.notify();
    }

    /// A worker failure is not a missing CLI, and must not leave a Checking badge forever.
    pub fn set_health_error(&mut self, cx: &mut Context<Self>, detail: &str) {
        for row in &mut self.rows {
            row.tone = RowTone::Bad;
            row.badge = "Check failed".into();
            row.detail = detail.into();
            row.path = None;
        }
        cx.notify();
    }

    pub fn set_tone(&mut self, tone: BaseTone) {
        self.draft.base_tone = tone;
        self.preview();
    }

    pub fn set_appearance(&mut self, appearance: AppearanceMode) {
        self.draft.appearance = appearance;
        self.preview();
    }

    pub fn set_accent(&mut self, accent: AccentColor) {
        self.draft.accent = accent;
        self.preview();
    }

    pub fn set_text_size(&mut self, text_size: TextSizePreset) {
        self.draft.text_size = text_size;
        self.preview();
    }

    fn preview(&self) {
        global::set(self.draft.apply_to(&global::current()));
    }

    /// Restore the opening appearance without persisting the draft.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.draft = self.baseline;
        if let Some(editor) = &self.provider_defaults {
            editor.update(cx, |editor, cx| editor.reset(cx));
        }
        self.provider_defaults_dirty = false;
        if let Some(editor) = &self.title_settings {
            editor.update(cx, |editor, cx| editor.reset(cx));
        }
        self.title_settings_dirty = false;
        if let Some(editor) = &self.follow_up {
            editor.update(cx, |editor, cx| editor.reset(cx));
        }
        self.follow_up_dirty = false;
        self.preview();
        cx.refresh_windows();
        cx.notify();
    }

    /// Apply the draft and report it.
    fn confirm(&mut self, window: &mut Window, cx: &mut App) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.dismiss_provider_menu(cx);
        (self.on_apply)(self.draft, window, cx);
    }

    /// Back out and restore the appearance present when the sheet opened.
    fn dismiss(&mut self, window: &mut Window, cx: &mut App) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.dismiss_provider_menu(cx);
        global::set(self.baseline.apply_to(&global::current()));
        cx.refresh_windows();
        (self.on_dismiss)(window, cx);
    }

    /// Ask the caller to look for the CLIs again.
    ///
    /// No `settled` guard: once the sheet has reported an outcome it draws
    /// nothing, so there is no control left to reach this through.
    pub(crate) fn recheck(&mut self, window: &mut Window, cx: &mut App) {
        (self.on_recheck)(window, cx);
    }
}

impl Render for SettingsSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();

        if self.settled {
            // Draw nothing rather than a dead sheet. The caller owns the unmount,
            // and until it happens an inert sheet that still looks live is one
            // more click away from applying the same draft again.
            return div().w(px(0.0)).h(px(0.0)).into_any_element();
        }

        let backdrop = div()
            .absolute()
            .size_full()
            // `overlay`, not `overlay_light`: a scrim over the whole window is
            // the strongest thing the token layer's overlays are asked to do.
            .bg(Colors::overlay(&theme))
            .on_mouse_down(MouseButton::Left, {
                let this = this.clone();
                move |_event, window, cx| {
                    this.update(cx, |sheet, cx| {
                        sheet.dismiss(window, cx);
                        cx.notify();
                    });
                }
            });

        // As large as a preferences window, and never closer to the window's
        // edge than a margin: the backdrop around it is how the reader backs
        // out with the pointer.
        let viewport = window.viewport_size();
        let width = px(Layout::SETTINGS_WINDOW_WIDTH)
            .min(viewport.width - px(Spacing::XXL * 2.0))
            .max(px(Layout::SETTINGS_NAV_WIDTH * 2.0));
        let height = px(Layout::SETTINGS_WINDOW_HEIGHT)
            .min(viewport.height - px(Spacing::HUGE * 2.0))
            .max(px(Spacing::HUGE * 4.0));
        let sheet = div()
            .w(width)
            // Padding around the panel so the shadow has somewhere to land: a
            // styled element's shadow is clipped to the element's own bounds.
            .p(px_len(Spacing::SM))
            .fx_shadow(Shadow::dialog(Colors::overlay(&theme)))
            // Swallow the click that lands on the sheet's own padding, so
            // reaching for a control does not read as backing out.
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(
                div()
                    .w_full()
                    .h(height - px(Spacing::SM * 2.0))
                    .bg(Colors::content_bg(&theme))
                    .rounded(px_len(Radii::XL))
                    .border(px_len(BorderWidth::HAIRLINE))
                    .border_color(Colors::border_medium(&theme))
                    .child(self.body(&this, cx)),
            );

        div()
            .id("settings-overlay")
            .occlude()
            .absolute()
            .size_full()
            .child(backdrop)
            .child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    // `.id` upgrades to `Stateful<Div>`, which is what carries
                    // `track_focus` and `on_key_down` in GPUI 0.2.2.
                    .id(SHEET_ID)
                    .track_focus(&self.focus)
                    .capture_key_down({
                        let this = this.clone();
                        move |event: &KeyDownEvent, window, cx| {
                            if this.read(cx).settings_menu_is_open(cx) {
                                // The app's one menu layer owns Escape, Enter,
                                // and arrow keys until this picker closes.
                                return;
                            }
                            let sheet = this.read(cx);
                            let search = sheet.search.read(cx);
                            let searching = search.focus_handle().is_focused(window);
                            let action = match event.keystroke.key.as_str() {
                                // A search is cleared before it is left.
                                "escape" if searching && !search.value().is_empty() => {
                                    Some(Action::ClearSearch)
                                }
                                "escape" => Some(Action::Dismiss),
                                "enter" if searching => Some(Action::OpenFirstMatch),
                                // Searching archives must not submit the settings.
                                "enter" if sheet.page != SettingsPage::Archives => {
                                    Some(Action::Confirm)
                                }
                                // Only while no field has the keys, so a slash
                                // can still be typed into one.
                                "/" if !event.keystroke.modifiers.modified()
                                    && sheet.focus.is_focused(window) =>
                                {
                                    Some(Action::Search)
                                }
                                _ => None,
                            };
                            let Some(action) = action else {
                                // Not claimed: swallowing Tab while the sheet
                                // had focus would strand the reader in it.
                                return;
                            };
                            cx.stop_propagation();
                            this.update(cx, |sheet, cx| {
                                match action {
                                    Action::Dismiss => sheet.dismiss(window, cx),
                                    Action::Confirm => sheet.confirm(window, cx),
                                    Action::ClearSearch => sheet.clear_search(cx),
                                    Action::OpenFirstMatch => sheet.open_first_match(window, cx),
                                    Action::Search => {
                                        sheet.search.read(cx).focus_handle().focus(window)
                                    }
                                }
                                cx.notify();
                            });
                        }
                    })
                    .child(sheet),
            )
            .into_any_element()
    }
}

/// What a key asks the sheet for: one of its two outcomes, or a move in and
/// out of the navigation's search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Confirm,
    Dismiss,
    ClearSearch,
    OpenFirstMatch,
    Search,
}

impl SettingsSheet {
    /// Navigation, header and footer stay put while the selected page scrolls.
    fn body(&self, this: &Entity<SettingsSheet>, cx: &App) -> impl IntoElement {
        let theme = global::current();
        let for_reset = this.clone();
        let for_done = this.clone();
        let for_close = this.clone();
        let content = match self.page {
            SettingsPage::ChatDefaults => div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::XXXL))
                .children(self.provider_defaults.clone())
                .children(self.follow_up.clone())
                .children(self.title_settings.clone())
                .into_any_element(),
            SettingsPage::Appearance => self.appearance_section(this),
            SettingsPage::Providers => self.provider_section(this),
            SettingsPage::Shortcuts => shortcuts_section(),
            SettingsPage::Archives => settings_section(
                "Archived chats",
                Some("Find and restore chats from your projects.".into()),
                div().w_full().children(self.archives.clone()),
            )
            .into_any_element(),
        };
        let page = self.page;
        let dirty = self.is_dirty();
        let header = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px_len(Spacing::MD))
            .px(px_len(Spacing::XXL))
            .h(px_len(Layout::PANE_HEADER_HEIGHT + Spacing::SM))
            .flex_shrink_0()
            .child(breadcrumb(&["Settings".into(), page.label().into()]))
            .child(icon_button(
                IconButtonProps::new(Icon::XMARK, "Close settings"),
                move |_, window, cx| {
                    for_close.update(cx, |sheet, cx| {
                        sheet.dismiss(window, cx);
                        cx.notify();
                    });
                },
            ));
        let footer = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px_len(Spacing::MD))
            .px(px_len(Spacing::XXL))
            .py(px_len(Spacing::MD))
            .flex_shrink_0()
            .child(
                div()
                    .min_w_0()
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .when(dirty, |note| note.child("Unsaved changes")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px_len(Spacing::SM))
                    .child(button_with_icon(
                        "Reset changes",
                        Some(Icon::ARROW_COUNTERCLOCKWISE),
                        ButtonStyle::Ghost,
                        self.can_reset(),
                        move |_, _, cx| {
                            for_reset.update(cx, |sheet, cx| sheet.reset(cx));
                        },
                    ))
                    .child(button(
                        "Done",
                        ButtonStyle::Primary,
                        true,
                        move |_, window, cx| {
                            for_done.update(cx, |sheet, cx| {
                                sheet.confirm(window, cx);
                                cx.notify();
                            });
                        },
                    )),
            );
        div()
            .size_full()
            .min_w_0()
            .flex()
            .child(self.navigation(this, cx))
            .child(divider_with_axis(DividerAxis::Vertical))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(header)
                    .child(divider())
                    .child(
                        div()
                            .id(SharedString::from(format!("settings-page-{}", page.key())))
                            .debug_selector(move || format!("settings-page-{}", page.key()))
                            .flex_1()
                            .min_h(px(0.0))
                            .w_full()
                            .min_w_0()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .justify_center()
                                    .px(px_len(Spacing::XXL))
                                    .py(px_len(Spacing::XXL))
                                    .child(
                                        div()
                                            .w_full()
                                            .max_w(px_len(Layout::SETTINGS_CONTENT_WIDTH))
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px_len(Spacing::XXXL))
                                            .child(content),
                                    ),
                            ),
                    )
                    .child(divider())
                    .child(footer),
            )
    }

    fn navigation(&self, this: &Entity<SettingsSheet>, cx: &App) -> AnyElement {
        let theme = global::current();
        let query = self.search_query(cx);
        let mut navigation = div()
            .debug_selector(|| "settings-navigation".into())
            .w(px_len(Layout::SETTINGS_NAV_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XXXS))
            .p(px_len(Spacing::MD))
            .bg(Colors::sidebar_bg(&theme))
            // The panel's corners; the content side keeps its own fill.
            .rounded_l(px_len(Radii::XL))
            .child(
                div()
                    .debug_selector(|| "settings-search".into())
                    .pb(px_len(Spacing::SM))
                    .child(settings_search(self.search.clone(), "/")),
            );
        let mut shown = 0;
        for (label, pages) in SettingsPage::GROUPS {
            let pages: Vec<_> = pages
                .iter()
                .copied()
                .filter(|page| self.page_available(*page) && page.matches(&query))
                .collect();
            if pages.is_empty() {
                continue;
            }
            if let Some(label) = label {
                navigation = navigation.child(settings_nav_label(label));
            }
            for page in pages {
                shown += 1;
                let this = this.clone();
                let selected = self.page == page;
                let item = div()
                    .w_full()
                    .debug_selector(move || format!("settings-nav-{}", page.key()))
                    .child(navigation_button(
                        page.label(),
                        page.icon(),
                        selected,
                        move |_, window, cx| {
                            this.update(cx, |sheet, cx| sheet.select_page(page, window, cx));
                        },
                    ));
                // Keep the recovery entry's established automation selector.
                navigation = if page == SettingsPage::Archives {
                    navigation.child(
                        div()
                            .debug_selector(|| "manage-archived-chats".into())
                            .child(item),
                    )
                } else {
                    navigation.child(item)
                };
            }
        }
        if shown == 0 {
            navigation = navigation.child(
                div()
                    .px(px_len(Spacing::MD))
                    .py(px_len(Spacing::SM))
                    .fx_caption()
                    .text_color(Colors::fg_tertiary(&theme))
                    .child("No matching settings"),
            );
        }
        navigation.into_any_element()
    }

    /// Provider CLIs: one row per report, and the re-check.
    fn provider_section(&self, this: &Entity<SettingsSheet>) -> AnyElement {
        let theme = global::current();
        let for_recheck = this.clone();

        let rows: Vec<AnyElement> = if self.rows.is_empty() {
            vec![
                settings_row_custom(
                    div()
                        .fx_body()
                        .text_color(Colors::fg_secondary(&theme))
                        .child("No provider has been looked for yet."),
                    div(),
                )
                .into_any_element(),
            ]
        } else {
            self.rows
                .iter()
                .map(|row| {
                    settings_row_custom(
                        provider_row(row, &theme),
                        badge(
                            SharedString::from(row.badge.clone()),
                            badge_tone(row.tone),
                            true,
                        ),
                    )
                    .into_any_element()
                })
                .collect()
        };

        settings_section_with_action(
            "Provider CLIs",
            Some("FlowX runs the Codex and Claude Code installed on this Mac.".into()),
            Some(
                button_with_icon(
                    "Check again",
                    Some(Icon::ARROW_CLOCKWISE),
                    ButtonStyle::Ghost,
                    // Never disabled: someone who has just installed a CLI
                    // should not have to relaunch to find out.
                    true,
                    move |_event, window, cx| {
                        for_recheck.update(cx, |sheet, cx| {
                            sheet.recheck(window, cx);
                            cx.notify();
                        });
                    },
                )
                .into_any_element(),
            ),
            settings_group(rows),
        )
        .into_any_element()
    }

    /// The theme's four choices, as two groups: color, then text.
    fn appearance_section(&self, this: &Entity<SettingsSheet>) -> AnyElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XXXL))
            .child(settings_section(
                "Theme",
                None,
                settings_group([
                    settings_row(
                        "Appearance",
                        Some("Follow the system, or stay light or dark.".into()),
                        choice_control(
                            &appearance_choices(),
                            &self.draft.appearance,
                            this,
                            SettingsSheet::set_appearance,
                        ),
                    )
                    .into_any_element(),
                    settings_row(
                        "Base color",
                        Some("The neutral tone behind every surface.".into()),
                        choice_control(
                            &tone_choices(),
                            &self.draft.base_tone,
                            this,
                            SettingsSheet::set_tone,
                        ),
                    )
                    .into_any_element(),
                    settings_row(
                        "Accent",
                        Some("Selection, focus and primary buttons.".into()),
                        choice_control(
                            &accent_choices(),
                            &self.draft.accent,
                            this,
                            SettingsSheet::set_accent,
                        ),
                    )
                    .into_any_element(),
                ]),
            ))
            .child(settings_section(
                "Text",
                None,
                settings_group([settings_row(
                    "Text size",
                    Some("Scales every text style in the app.".into()),
                    choice_control(
                        &text_size_choices(),
                        &self.draft.text_size,
                        this,
                        SettingsSheet::set_text_size,
                    ),
                )
                .into_any_element()]),
            ))
            .into_any_element()
    }
}

/// Every key FlowX binds, grouped as the menus group them.
///
/// Drawn from the same table that registers the keys, so the page cannot list a
/// shortcut that does nothing or miss one that does.
fn shortcuts_section() -> AnyElement {
    let theme = global::current();
    let table = crate::commands::key_bindings();
    let mut section = div().w_full().flex().flex_col().gap(px_len(Spacing::XXXL));
    let mut first = true;
    for group in crate::commands::ShortcutGroup::ALL {
        let rows: Vec<AnyElement> = table
            .iter()
            .filter(|shortcut| shortcut.group == group)
            .map(|shortcut| {
                let keys = shortcut.symbols();
                let selector = format!("shortcut:{keys}");
                settings_row_dense(
                    div()
                        .fx_body()
                        .text_color(Colors::fg(&theme))
                        .child(SharedString::from(shortcut.label.clone())),
                    fx_design::components::shortcut_chip(keys),
                )
                .debug_selector(move || selector.clone())
                .into_any_element()
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        let description = first.then(|| "The menus show the same keys beside each command.".into());
        first = false;
        section = section.child(settings_section(
            group.title(),
            description,
            settings_group(rows),
        ));
    }
    section.into_any_element()
}

/// One provider: its name, where it was found, and what to do.
fn provider_row(row: &HealthRow, theme: &Theme) -> AnyElement {
    let mut column = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXXS))
        .child(
            div()
                .fx_body_medium()
                .text_color(Colors::fg(theme))
                .child(SharedString::from(row.title.clone())),
        );

    if let Some(path) = &row.path {
        let path_selector = format!("provider-path-{}", row.id);
        let full_path = SharedString::from(path.clone());
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let shown_path = runtime_path_label(path, home.as_deref());
        column = column.child(
            div()
                .id(SharedString::from(format!("provider-path-{}", row.id)))
                .debug_selector(move || path_selector.clone())
                .w_full()
                .min_w_0()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_secondary(theme))
                .whitespace_nowrap()
                .overflow_hidden()
                .child(SharedString::from(shown_path))
                .tooltip(move |window, cx| tooltip(full_path.clone(), window, cx)),
        );
    }

    column
        .child(
            div()
                .fx_caption()
                .text_color(Colors::fg_secondary(theme))
                .child(SharedString::from(row.detail.clone())),
        )
        .into_any_element()
}

/// Keep path labels stable across GPUI's intrinsic and final width passes.
/// The tooltip always retains the full path; the row favors the installation
/// root and the final two components over unhelpful intermediate directories.
fn runtime_path_label(path: &str, home: Option<&Path>) -> String {
    const MAX_CHARS: usize = 42;
    let shown = home
        .and_then(|home| Path::new(path).strip_prefix(home).ok())
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|| path.to_string());
    if shown.chars().count() <= MAX_CHARS {
        return shown;
    }
    let parts: Vec<_> = shown.split('/').collect();
    let prefix = parts.iter().take(2).copied().collect::<Vec<_>>().join("/");
    for keep in [2, 1] {
        if parts.len() > keep + 2 {
            let suffix = parts[parts.len() - keep..].join("/");
            let label = format!("{prefix}/…/{suffix}");
            if label.chars().count() <= MAX_CHARS {
                return label;
            }
        }
    }
    // A single unusually long filename still needs a bounded label. Work in
    // characters, so shortening a Unicode path never splits a code point.
    let chars: Vec<_> = shown.chars().collect();
    let prefix: String = chars.iter().take(12).collect();
    let suffix: String = chars[chars.len() - (MAX_CHARS - 13)..].iter().collect();
    format!("{prefix}…{suffix}")
}

/// A group of exclusive choices, for the trailing edge of a settings row.
///
/// `select` is a plain function pointer so one body serves all four groups: the
/// alternative is a generic closure type per call site and four near-identical
/// bodies.
fn choice_control<T: Clone + PartialEq + 'static>(
    choices: &[Choice<T>],
    selected: &T,
    this: &Entity<SettingsSheet>,
    select: fn(&mut SettingsSheet, T),
) -> AnyElement {
    let options: Vec<SegmentedOption<T>> = choices
        .iter()
        .map(|choice| SegmentedOption::new(choice.value.clone(), choice.label))
        .collect();
    // Cloned out before the closure: the segmented control's handler is
    // `'static`, and a borrow of the caller's entity cannot outlive this call.
    let for_select = this.clone();

    segmented_control(&options, selected, move |value, _window, cx| {
        let this = for_select.clone();
        this.update(cx, |sheet, cx| {
            select(sheet, value.clone());
            cx.refresh_windows();
            cx.notify();
        });
    })
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_design::theme::ThemeGuard;
    use gpui::{AnyWindowHandle, Keystroke, Modifiers, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    // ── The draft ───────────────────────────────────────────────────────

    #[test]
    fn a_draft_taken_from_a_theme_reads_back_those_four_settings() {
        let theme = Theme::new(
            AppearanceMode::Light,
            BaseTone::Stone,
            AccentColor::Rose,
            TextSizePreset::Large,
        );
        let draft = SettingsDraft::from_theme(&theme);

        assert_eq!(draft.base_tone, BaseTone::Stone);
        assert_eq!(draft.appearance, AppearanceMode::Light);
        assert_eq!(draft.accent, AccentColor::Rose);
        assert_eq!(draft.text_size, TextSizePreset::Large);
    }

    #[test]
    fn a_draft_that_equals_the_current_settings_is_not_dirty() {
        // Otherwise Done and Reset are both live on a sheet nobody has touched.
        let theme = Theme::default();
        let current = SettingsDraft::from_theme(&theme);
        assert!(!current.is_dirty_against(&current));
        assert!(!SettingsDraft::from_theme(&theme).is_dirty_against(&current));
    }

    #[test]
    fn a_draft_differing_only_in_text_size_is_dirty() {
        // Text size is the one setting that changes no colour, which makes it
        // the one a colour-only comparison drops — and the reader who then
        // presses Done sees nothing happen.
        let theme = Theme::default();
        let mut draft = SettingsDraft::from_theme(&theme);
        draft.text_size = TextSizePreset::Comfortable;

        // Nothing else moved, so this is genuinely a text-size-only difference.
        assert_eq!(
            draft.base_tone, theme.base_tone,
            "the premise of this test is that only text size changed"
        );
        assert!(draft.is_dirty_against(&SettingsDraft::from_theme(&theme)));
    }

    #[test]
    fn every_field_makes_a_draft_dirty() {
        let theme = Theme::default();
        let current = SettingsDraft::from_theme(&theme);

        let other_tone = BaseTone::ALL
            .iter()
            .find(|tone| **tone != theme.base_tone)
            .copied();
        let other_mode = AppearanceMode::ALL
            .iter()
            .find(|mode| **mode != theme.appearance_mode)
            .copied();
        let other_accent = AccentColor::ALL
            .iter()
            .find(|accent| **accent != theme.accent_color)
            .copied();
        let other_size = TextSizePreset::ALL
            .iter()
            .find(|size| **size != theme.text_size)
            .copied();

        for (label, changed) in [
            (
                "tone",
                other_tone.map(|tone| {
                    let mut draft = current;
                    draft.base_tone = tone;
                    draft
                }),
            ),
            (
                "appearance",
                other_mode.map(|mode| {
                    let mut draft = current;
                    draft.appearance = mode;
                    draft
                }),
            ),
            (
                "accent",
                other_accent.map(|accent| {
                    let mut draft = current;
                    draft.accent = accent;
                    draft
                }),
            ),
            (
                "text size",
                other_size.map(|size| {
                    let mut draft = current;
                    draft.text_size = size;
                    draft
                }),
            ),
        ] {
            let changed = changed.unwrap_or_else(|| panic!("{label} had no alternative"));
            assert!(
                changed.is_dirty_against(&current),
                "changing only {label} went unnoticed"
            );
        }
    }

    #[test]
    fn applying_a_draft_keeps_the_resolved_system_appearance() {
        // `Theme` has five fields and the draft owns four. Building a default
        // and overwriting four would reset the fifth, which is the OS
        // appearance — a light-mode window would repaint dark on Done.
        let mut light_system = Theme::new(
            AppearanceMode::System,
            BaseTone::Zinc,
            AccentColor::Violet,
            TextSizePreset::Standard,
        );
        light_system.system_appearance = false;

        let draft = SettingsDraft::from_theme(&light_system);
        let applied = draft.apply_to(&light_system);

        assert!(!applied.system_appearance);
        assert!(!applied.is_dark(), "a light system repainted dark");
    }

    #[test]
    fn applying_a_draft_keeps_the_settings_it_does_not_own() {
        let theme = Theme::new(
            AppearanceMode::Light,
            BaseTone::Zinc,
            AccentColor::Violet,
            TextSizePreset::Standard,
        );
        let mut draft = SettingsDraft::from_theme(&theme);
        draft.accent = AccentColor::Emerald;
        draft.text_size = TextSizePreset::Compact;

        let applied = draft.apply_to(&theme);
        assert_eq!(applied.accent_color, AccentColor::Emerald);
        assert_eq!(applied.text_size, TextSizePreset::Compact);
        assert_eq!(applied.base_tone, BaseTone::Zinc);
        assert_eq!(applied.appearance_mode, AppearanceMode::Light);
    }

    // ── Choice tables ───────────────────────────────────────────────────

    #[test]
    fn every_setting_reaches_a_segment_and_is_recovered_from_its_label() {
        for tone in BaseTone::ALL {
            assert_eq!(tone_from_label(tone.label()), Some(tone), "{tone:?}");
        }
        for mode in AppearanceMode::ALL {
            assert_eq!(appearance_from_label(mode.label()), Some(mode));
        }
        for accent in AccentColor::ALL {
            assert_eq!(accent_from_label(accent.label()), Some(accent));
        }
        for size in TextSizePreset::ALL {
            assert_eq!(text_size_from_label(size.label()), Some(size));
        }
    }

    #[test]
    fn an_unknown_tone_label_is_refused_rather_than_defaulted() {
        // The rule that keeps a stored choice from becoming a live one by
        // accident: a tone upstream has dropped must not resolve to whatever
        // the app happens to default to.
        assert_eq!(tone_from_label("Sepia"), None);
        assert_eq!(tone_from_label(""), None);
        assert_eq!(tone_from_label(""), None);
        // Exact match, case included — no control ever renders a lowercase
        // label, so accepting one would accept nothing the reader can see.
        assert_eq!(tone_from_label("slate"), None);
        assert_eq!(tone_from_label(" Slate"), None);
    }

    #[test]
    fn the_tables_agree_with_the_design_systems_own_labels() {
        // The tables are built by iterating `ALL`; this checks them against the
        // per-variant `label()` arms instead, which are written separately. If a
        // label and a table drifted, one of the two would be lying.
        for choices in [
            tone_choices()
                .iter()
                .map(|c| (c.label, c.value.label()))
                .collect::<Vec<_>>(),
            appearance_choices()
                .iter()
                .map(|c| (c.label, c.value.label()))
                .collect::<Vec<_>>(),
            accent_choices()
                .iter()
                .map(|c| (c.label, c.value.label()))
                .collect::<Vec<_>>(),
            text_size_choices()
                .iter()
                .map(|c| (c.label, c.value.label()))
                .collect::<Vec<_>>(),
        ] {
            assert!(!choices.is_empty());
            for (from_table, from_arm) in choices {
                assert_eq!(from_table, from_arm);
            }
        }
    }

    #[test]
    fn no_two_segments_on_the_sheet_share_a_label() {
        // `segmented_control` builds its element id from the title alone, so two
        // groups offering the same label would share one id and the second would
        // silently never be clicked.
        let mut labels: Vec<&str> = Vec::new();
        labels.extend(tone_choices().iter().map(|c| c.label));
        labels.extend(appearance_choices().iter().map(|c| c.label));
        labels.extend(accent_choices().iter().map(|c| c.label));
        labels.extend(text_size_choices().iter().map(|c| c.label));

        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn each_table_holds_every_value_exactly_once() {
        let tones = tone_choices();
        assert_eq!(tones.len(), BaseTone::ALL.len());
        for tone in BaseTone::ALL {
            let offered = tones.iter().filter(|choice| choice.value == tone).count();
            assert_eq!(offered, 1, "{tone:?} is offered {offered} times");
        }

        let sizes = text_size_choices();
        for size in TextSizePreset::ALL {
            let offered = sizes.iter().filter(|choice| choice.value == size).count();
            assert_eq!(offered, 1, "{size:?} is offered {offered} times");
        }
    }

    // ── Provider health ─────────────────────────────────────────────────

    /// A spec pointing at `path`, with a hint, so a row has something to say.
    fn spec_at(path: &Path) -> BinarySpec {
        BinarySpec::new("test", "Test", vec![path.to_string_lossy().into_owned()])
            .install_hint("Install it with `npm install -g something`.")
    }

    fn rows_for(spec: &BinarySpec, health: &BinaryHealth) -> Vec<HealthRow> {
        health_rows(&[ProviderReport::new(spec, health)])
    }

    fn available(path: &str) -> BinaryHealth {
        BinaryHealth::Available {
            path: path.to_string(),
            version: Some("1.2.3".into()),
        }
    }

    #[test]
    fn a_found_and_healthy_binary_is_ok() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(&spec, &available("/opt/homebrew/bin/codex"));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tone, RowTone::Ok);
        assert_eq!(rows[0].badge, "Ready");
        assert_eq!(rows[0].detail, "Test 1.2.3");
        assert_eq!(rows[0].path.as_deref(), Some("/opt/homebrew/bin/codex"));
        assert!(rows[0].path_is_actionable());
    }

    #[test]
    fn an_installed_binary_with_no_version_read_still_says_it_is_installed() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(
            &spec,
            &BinaryHealth::Available {
                path: "/opt/homebrew/bin/codex".into(),
                version: None,
            },
        );

        assert_eq!(rows[0].tone, RowTone::Ok);
        assert!(rows[0].detail.contains("is installed"));
    }

    #[test]
    fn a_missing_binary_is_not_an_error_the_reader_can_act_on() {
        // The rule the whole tone scale rests on: a red row for "codex is not
        // installed" is a permanent red, and a permanent red stops being read.
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("absent"));
        let rows = rows_for(&spec, &BinaryHealth::NotFound);

        assert_eq!(rows[0].tone, RowTone::Idle);
        assert_ne!(rows[0].tone, RowTone::Bad);
        assert_eq!(rows[0].badge, "Not installed");
        assert_eq!(rows[0].path, None);
        // And it still says the one useful thing.
        assert!(rows[0].detail.contains("npm install"), "{}", rows[0].detail);
    }

    #[test]
    fn a_missing_binary_without_a_hint_still_says_it_is_missing() {
        let spec = BinarySpec::new("test", "Test", vec!["/nope/absent".into()]);
        let rows = rows_for(&spec, &BinaryHealth::NotFound);

        assert_eq!(rows[0].tone, RowTone::Idle);
        assert_eq!(rows[0].detail, "Test was not found.");
    }

    #[test]
    fn a_quarantined_binary_is_a_warning_that_says_what_to_do() {
        // Warn, not bad: the binary is installed and one right-click clears it.
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(
            &spec,
            &BinaryHealth::Quarantined {
                path: "/Users/someone/.local/bin/codex".into(),
            },
        );

        assert_eq!(rows[0].tone, RowTone::Warn);
        assert_ne!(rows[0].tone, RowTone::Bad);
        assert_eq!(rows[0].badge, "Not approved");
        let detail = &rows[0].detail;
        assert!(detail.contains("approved"), "{detail}");
        assert!(detail.contains("Finder"), "{detail}");
        // A different instruction from installing it: the binary is already
        // there, and telling someone to install it sends them the wrong way.
        assert!(!detail.contains("npm install"), "{detail}");
    }

    #[test]
    fn a_folder_where_the_binary_should_be_is_bad_and_says_so() {
        // The same `NotFound` discovery reports for a directory and for nothing
        // at all, and the two need opposite instructions.
        let temp = tempfile::tempdir().expect("a temp dir");
        let shadow = temp.path().join("codex");
        std::fs::create_dir(&shadow).expect("the folder was created");
        let spec = spec_at(&shadow);

        let rows = rows_for(&spec, &BinaryHealth::NotFound);
        assert_eq!(rows[0].tone, RowTone::Bad);
        assert_eq!(rows[0].badge, "Not a program");
        let detail = &rows[0].detail;
        assert!(detail.contains("folder"), "{detail}");
        assert!(detail.contains("codex"), "{detail}");
        // "Not installed" is not what is wrong here, and the install hint would
        // send the reader to install something that is already installed.
        assert!(!detail.contains("npm install"), "{detail}");
        assert!(rows[0].path_is_actionable(), "they can move a folder");
    }

    #[test]
    fn a_binary_found_behind_a_folder_is_still_ok() {
        // Discovery skips a directory and takes the file behind it, so the
        // folder must not turn a working provider into a red one.
        let temp = tempfile::tempdir().expect("a temp dir");
        let directory = temp.path().join("bin");
        std::fs::create_dir(&directory).expect("the folder was created");
        let binary = directory.join("codex");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("the script was written");
        let spec = spec_at(&directory);

        let rows = rows_for(&spec, &available(&binary.to_string_lossy()));
        assert_eq!(rows[0].tone, RowTone::Ok);
    }

    #[test]
    fn a_health_that_is_still_being_checked_is_not_a_problem() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(&spec, &BinaryHealth::Checking);

        assert_eq!(rows[0].tone, RowTone::Idle);
        assert_eq!(rows[0].badge, "Checking");
        assert_eq!(rows[0].path, None);
    }

    #[test]
    fn both_providers_can_be_present_at_once() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let codex = BinarySpec::new("codex", "Codex", vec!["/bin/codex".into()]);
        let claude = spec_at(&temp.path().join("tool"));
        let codex_health = available("/bin/codex");
        let claude_health = available("/bin/claude");
        let reports = [
            ProviderReport::new(&codex, &codex_health),
            ProviderReport::new(&claude, &claude_health),
        ];

        let rows = health_rows(&reports);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.tone == RowTone::Ok));
        assert_eq!(rows[0].title, "Codex");
        assert_eq!(rows[1].title, "Test");
    }

    #[test]
    fn both_providers_can_be_absent_at_once() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let codex = spec_at(&temp.path().join("no-codex"));
        let claude = spec_at(&temp.path().join("no-claude"));
        let not_found = BinaryHealth::NotFound;
        let reports = [
            ProviderReport::new(&codex, &not_found),
            ProviderReport::new(&claude, &not_found),
        ];

        let rows = health_rows(&reports);
        assert_eq!(rows.len(), 2);
        // Two missing providers are not two errors.
        assert!(rows.iter().all(|row| row.tone == RowTone::Idle));
    }

    #[test]
    fn two_reports_of_the_same_binary_are_both_shown() {
        // The rows are rendered directly, so folding them would hide the
        // reading that matters whenever the first one was the stale one.
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let stale = BinaryHealth::NotFound;
        let fresh = available("/bin/codex");
        let reports = [
            ProviderReport::new(&spec, &stale),
            ProviderReport::new(&spec, &fresh),
        ];

        let rows = health_rows(&reports);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tone, RowTone::Idle);
        assert_eq!(rows[1].tone, RowTone::Ok);
    }

    #[test]
    fn rows_come_out_in_the_order_they_went_in() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let codex = BinarySpec::new(
            "codex",
            "Codex",
            vec![temp.path().join("codex").to_string_lossy().into_owned()],
        );
        let claude = BinarySpec::new("claude", "Claude Code", vec!["/bin/claude".into()]);
        let claude_health = available("/bin/claude");
        let codex_health = BinaryHealth::NotFound;
        let reports = [
            ProviderReport::new(&claude, &claude_health),
            ProviderReport::new(&codex, &codex_health),
        ];

        let rows = health_rows(&reports);
        assert_eq!(rows[0].id, "claude");
        assert_eq!(rows[1].id, "codex");
        assert_eq!(rows[0].tone, RowTone::Ok, "the order was not preserved");
        assert_eq!(rows[1].tone, RowTone::Idle);

        // And the same input always produces the same rows.
        assert_eq!(health_rows(&reports), rows);
    }

    #[test]
    fn an_empty_report_list_produces_no_rows() {
        let none: Vec<ProviderReport<'_>> = Vec::new();
        assert!(health_rows(&none).is_empty());
    }

    #[test]
    fn a_path_inside_an_app_bundle_is_not_one_the_reader_can_be_sent_to() {
        assert!(is_actionable_path("/opt/homebrew/bin/codex"));
        assert!(is_actionable_path("/Users/someone/.local/bin/claude"));
        assert!(!is_actionable_path(
            "/Applications/Codex.app/Contents/MacOS/codex"
        ));
        // Relative is not actionable either: it would resolve against a working
        // directory the reader never chose.
        assert!(!is_actionable_path("bin/codex"));
        assert!(!is_actionable_path(""));
    }

    #[test]
    fn a_bundled_binary_is_still_reported_with_its_path() {
        // The row carries what discovery found; the *copy* is what must not make
        // a promise about it.
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(
            &spec,
            &BinaryHealth::Quarantined {
                path: "/Applications/Codex.app/Contents/MacOS/codex".into(),
            },
        );

        assert_eq!(rows[0].tone, RowTone::Warn);
        assert_eq!(
            rows[0].path.as_deref(),
            Some("/Applications/Codex.app/Contents/MacOS/codex")
        );
        assert!(!rows[0].path_is_actionable());
    }

    #[test]
    fn the_copy_for_a_bundled_binary_does_not_ask_the_reader_to_approve_it() {
        // Finder does not offer "Open" for a file inside a signed bundle, so the
        // usual quarantine sentence is an instruction with no action at the end
        // of it. The honest fix is reinstalling.
        let temp = tempfile::tempdir().expect("a temp dir");
        let spec = spec_at(&temp.path().join("tool"));
        let rows = rows_for(
            &spec,
            &BinaryHealth::Quarantined {
                path: "/Applications/Codex.app/Contents/MacOS/codex".into(),
            },
        );

        let detail = &rows[0].detail;
        assert!(!detail.contains("Finder"), "{detail}");
        assert!(!detail.contains("/Applications"), "{detail}");
        assert!(detail.contains("Reinstall"), "{detail}");
    }

    #[test]
    fn every_row_tone_gets_its_own_badge_tone() {
        let mut seen: Vec<BadgeTone> = Vec::new();
        for tone in RowTone::ALL {
            let badge = badge_tone(tone);
            assert!(!seen.contains(&badge), "{tone:?} shares a badge tone");
            seen.push(badge);
        }
        // Idle is neutral: "not installed" is not a result.
        assert_eq!(badge_tone(RowTone::Idle), BadgeTone::Neutral);
        assert_eq!(badge_tone(RowTone::Ok), BadgeTone::Success);
    }

    // ── The sheet ───────────────────────────────────────────────────────

    /// One-line view whose only job is to give the sheet a `Render` stack.
    struct Host {
        sheet: Entity<SettingsSheet>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.sheet.clone().into_any_element()
        }
    }

    struct Mounted {
        _theme_guard: ThemeGuard,
        test: TestAppContext,
        window: AnyWindowHandle,
        sheet: Entity<SettingsSheet>,
        applied: Rc<RefCell<Vec<SettingsDraft>>>,
        dismissed: Rc<RefCell<usize>>,
        rechecked: Rc<RefCell<usize>>,
    }

    impl Mounted {
        /// Read something out of the sheet.
        fn read<T>(&self, read: impl FnOnce(&SettingsSheet) -> T) -> T {
            self.test.read(|cx| read(self.sheet.read(cx)))
        }

        /// Change the draft the way a segment click would.
        fn choose(&mut self, edit: impl FnOnce(&mut SettingsSheet)) {
            self.sheet.update(&mut self.test, |sheet, cx| {
                edit(sheet);
                cx.notify();
            });
            self.test.refresh().expect("the sheet repainted");
        }

        /// Press Done, the way the footer button does.
        fn done(&mut self) {
            let sheet = self.sheet.clone();
            self.window
                .update(&mut self.test, move |_entity, window, cx| {
                    sheet.update(cx, |sheet, cx| {
                        sheet.confirm(window, cx);
                        cx.notify();
                    });
                })
                .expect("the window is still alive");
            self.test.refresh().expect("the sheet repainted");
        }

        /// Press Re-check, the way the provider header does.
        fn recheck(&mut self) {
            let sheet = self.sheet.clone();
            self.window
                .update(&mut self.test, move |_entity, window, cx| {
                    sheet.update(cx, |sheet, cx| {
                        sheet.recheck(window, cx);
                        cx.notify();
                    });
                })
                .expect("the window is still alive");
            self.test.refresh().expect("the sheet repainted");
        }

        /// Send a key to the window, as the platform would.
        fn press(&mut self, key: &str) {
            self.test
                .dispatch_keystroke(self.window, Keystroke::parse(key).expect("a parsable key"));
            self.test.refresh().expect("the sheet repainted");
        }

        /// Press at a point in the window, for the backdrop.
        fn click_at(&mut self, position: gpui::Point<gpui::Pixels>) {
            // Borrowed rather than held: `simulate_mouse_down` needs the visual
            // context mutably, and both borrow `test`.
            let window = self.window;
            let mut visual = VisualTestContext::from_window(window, &self.test);
            let modifiers = Modifiers::default();
            visual.simulate_mouse_down(position, MouseButton::Left, modifiers);
            visual.simulate_mouse_up(position, MouseButton::Left, modifiers);
            self.test.refresh().expect("the sheet repainted");
        }

        fn applied(&self) -> Vec<SettingsDraft> {
            self.applied.borrow().clone()
        }

        fn dismissed(&self) -> usize {
            *self.dismissed.borrow()
        }

        fn rechecked(&self) -> usize {
            *self.rechecked.borrow()
        }
    }

    fn mount() -> Mounted {
        let theme_guard = ThemeGuard::install(global::current());
        let mut test = TestAppContext::single();
        let applied: Rc<RefCell<Vec<SettingsDraft>>> = Rc::default();
        let dismissed: Rc<RefCell<usize>> = Rc::default();
        let rechecked: Rc<RefCell<usize>> = Rc::default();

        let sheet = test.update(|cx| {
            let for_apply = applied.clone();
            let for_dismiss = dismissed.clone();
            let for_recheck = rechecked.clone();
            SettingsSheet::new(
                cx,
                Box::new(move |draft, _window, _cx| {
                    for_apply.borrow_mut().push(draft);
                }),
                Box::new(move |_window, _cx| {
                    *for_dismiss.borrow_mut() += 1;
                }),
                Box::new(move |_window, _cx| {
                    *for_recheck.borrow_mut() += 1;
                }),
            )
        });

        let mounted = sheet.clone();
        let window = test.add_window(move |window, cx| {
            let focused = mounted.clone();
            focused.update(cx, |sheet, _| sheet.focus_handle().focus(window));
            Host { sheet: mounted }
        });

        test.refresh().expect("the sheet painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");

        Mounted {
            _theme_guard: theme_guard,
            window: window.into(),
            test,
            sheet,
            applied,
            dismissed,
            rechecked,
        }
    }

    #[test]
    fn an_untouched_sheet_has_nothing_to_apply() {
        // Otherwise Done and Reset are both live on a sheet nobody has touched.
        let mounted = mount();
        assert!(!mounted.read(|sheet| sheet.is_dirty()));
        assert!(!mounted.read(|sheet| sheet.can_reset()));
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn provider_drafts_are_independent_and_reset_with_the_sheet() {
        let mut mounted = mount();
        let editor = mounted.test.update(|cx| {
            let menus = cx.new(|_| fx_design::components::MenuLayer::default());
            crate::provider_settings::ProviderSettings::new(
                fx_store::ProviderDefaults::default(),
                fx_store::ProviderDefaults::default(),
                menus,
                cx,
            )
        });
        mounted.sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.set_provider_defaults(editor.clone(), cx)
        });
        editor.update(&mut mounted.test, |editor, cx| {
            assert!(editor.apply("settings-defaults:claude:access", "Supervised", cx));
        });
        mounted.test.refresh().unwrap();
        assert!(mounted.read(|sheet| sheet.can_reset()));
        mounted.test.read(|cx| {
            let (codex, claude) = editor.read(cx).values();
            assert_eq!(codex, fx_store::ProviderDefaults::default());
            assert_eq!(claude.agent_access, Some(fx_core::AgentAccess::Supervised));
        });
        mounted
            .sheet
            .update(&mut mounted.test, |sheet, cx| sheet.reset(cx));
        mounted.test.refresh().unwrap();
        assert!(!mounted.read(|sheet| sheet.is_dirty()));
        assert!(mounted.applied().is_empty());
        mounted.test.read(|cx| {
            assert_eq!(
                editor.read(cx).values(),
                (
                    fx_store::ProviderDefaults::default(),
                    fx_store::ProviderDefaults::default()
                )
            )
        });
    }

    #[test]
    fn chat_name_changes_follow_settings_reset_and_cancel() {
        let mut mounted = mount();
        let baseline = fx_store::AutoTitlePreferences::default();
        let editor = mounted.test.update(|cx| {
            let menus = cx.new(|_| fx_design::components::MenuLayer::default());
            crate::title_settings::TitleSettings::new(baseline.clone(), menus, cx)
        });
        mounted.sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.set_title_settings(editor.clone(), cx)
        });
        editor.update(&mut mounted.test, |editor, cx| {
            editor.set_enabled(false, cx);
            assert!(editor.apply("settings-titles:model", fx_core::DEFAULT_CODEX_MODEL_ID, cx));
        });
        mounted.test.refresh().unwrap();
        assert!(mounted.read(|sheet| sheet.can_reset()));
        mounted
            .sheet
            .update(&mut mounted.test, |sheet, cx| sheet.reset(cx));
        mounted.test.refresh().unwrap();
        assert!(!mounted.read(|sheet| sheet.is_dirty()));
        mounted
            .test
            .read(|cx| assert_eq!(editor.read(cx).values(), baseline));
        editor.update(&mut mounted.test, |editor, cx| {
            editor.set_enabled(false, cx)
        });
        mounted.press("escape");
        assert_eq!(mounted.dismissed(), 1);
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn settings_navigation_preserves_edits_and_keeps_page_geometry_stable() {
        let mut mounted = mount();
        let (menus, editor) = mounted.test.update(|cx| {
            let menus = cx.new(|_| fx_design::components::MenuLayer::default());
            let editor = crate::provider_settings::ProviderSettings::new(
                fx_store::ProviderDefaults::default(),
                fx_store::ProviderDefaults::default(),
                menus.clone(),
                cx,
            );
            (menus, editor)
        });
        mounted.sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.set_provider_defaults(editor.clone(), cx);
        });
        let mut visual = VisualTestContext::from_window(mounted.window, &mounted.test);
        visual.simulate_resize(gpui::size(px(1_000.0), px(640.0)));
        mounted.test.refresh().unwrap();
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::ChatDefaults);
        let defaults_bounds = visual.debug_bounds("settings-page-chat-defaults").unwrap();
        let navigation_bounds = visual.debug_bounds("settings-navigation").unwrap();
        assert!(defaults_bounds.size.width > px(400.0));
        assert!(navigation_bounds.right() <= defaults_bounds.left());

        editor.update(&mut mounted.test, |editor, cx| {
            editor.apply("settings-defaults:claude:access", "Supervised", cx);
        });
        menus.update(&mut mounted.test, |menus, cx| {
            menus.open(
                fx_design::components::MenuModel::default(),
                "settings-defaults:codex:model",
            );
            cx.notify();
        });
        let appearance = visual
            .debug_bounds("settings-nav-appearance")
            .unwrap()
            .center();
        mounted.click_at(appearance);
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::Appearance);
        mounted.test.read(|cx| assert!(!menus.read(cx).is_open()));
        assert_eq!(
            visual.debug_bounds("settings-page-appearance").unwrap(),
            defaults_bounds
        );
        mounted.choose(|sheet| sheet.set_tone(BaseTone::Stone));
        let providers = visual
            .debug_bounds("settings-nav-providers")
            .unwrap()
            .center();
        mounted.click_at(providers);
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::Providers);
        assert_eq!(
            visual.debug_bounds("settings-page-providers").unwrap(),
            defaults_bounds
        );
        assert_eq!(global::current().base_tone, BaseTone::Stone);
        let defaults = visual
            .debug_bounds("settings-nav-chat-defaults")
            .unwrap()
            .center();
        mounted.click_at(defaults);
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::ChatDefaults);
        mounted.test.read(|cx| {
            assert_eq!(
                editor.read(cx).values().1.agent_access,
                Some(fx_core::AgentAccess::Supervised)
            )
        });
        assert!(mounted.read(|sheet| sheet.is_dirty()));
        assert!(mounted.applied().is_empty());
        assert_eq!(
            visual.debug_bounds("settings-navigation").unwrap(),
            navigation_bounds
        );
    }

    #[test]
    fn escape_and_enter_do_not_settle_settings_while_a_provider_picker_is_open() {
        let mut mounted = mount();
        let (menus, editor) = mounted.test.update(|cx| {
            let menus = cx.new(|_| fx_design::components::MenuLayer::default());
            let editor = crate::provider_settings::ProviderSettings::new(
                fx_store::ProviderDefaults::default(),
                fx_store::ProviderDefaults::default(),
                menus.clone(),
                cx,
            );
            (menus, editor)
        });
        mounted.sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.set_provider_defaults(editor, cx)
        });
        menus.update(&mut mounted.test, |menus, cx| {
            menus.open(
                fx_design::components::MenuModel::default(),
                "settings-defaults:codex:model",
            );
            cx.notify();
        });
        mounted.press("escape");
        mounted.press("enter");
        assert_eq!(*mounted.dismissed.borrow(), 0);
        assert!(mounted.applied().is_empty());
        mounted.done();
        mounted.test.read(|cx| assert!(!menus.read(cx).is_open()));
        assert_eq!(mounted.applied().len(), 1);
    }

    #[test]
    fn choosing_a_setting_previews_the_theme_without_persisting_the_draft() {
        let theme = Theme::default();
        let _guard = ThemeGuard::install(theme);

        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_tone(BaseTone::Stone));

        assert!(mounted.read(|sheet| sheet.is_dirty()));
        assert!(mounted.read(|sheet| sheet.can_reset()));
        assert_eq!(global::current(), theme.with_tone(BaseTone::Stone));
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn every_setting_can_be_chosen() {
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_tone(BaseTone::Slate));
        assert_eq!(
            mounted.read(|sheet| sheet.draft().base_tone),
            BaseTone::Slate
        );

        mounted.choose(|sheet| sheet.set_appearance(AppearanceMode::Light));
        assert_eq!(
            mounted.read(|sheet| sheet.draft().appearance),
            AppearanceMode::Light
        );

        mounted.choose(|sheet| sheet.set_accent(AccentColor::Emerald));
        assert_eq!(
            mounted.read(|sheet| sheet.draft().accent),
            AccentColor::Emerald
        );

        mounted.choose(|sheet| sheet.set_text_size(TextSizePreset::Large));
        assert_eq!(
            mounted.read(|sheet| sheet.draft().text_size),
            TextSizePreset::Large
        );
    }

    #[test]
    fn done_applies_the_whole_draft_once() {
        let mut mounted = mount();
        mounted.choose(|sheet| {
            sheet.set_tone(BaseTone::Stone);
            sheet.set_accent(AccentColor::Orange);
            sheet.set_text_size(TextSizePreset::Compact);
        });

        mounted.done();
        let applied = mounted.applied();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].base_tone, BaseTone::Stone);
        assert_eq!(applied[0].accent, AccentColor::Orange);
        assert_eq!(applied[0].text_size, TextSizePreset::Compact);
        assert_eq!(mounted.dismissed(), 0);
    }

    #[test]
    fn a_second_done_does_not_apply_twice() {
        // The caller unmounts on the first callback and reacts to it, not the
        // second, so the sheet has to refuse the second itself.
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_accent(AccentColor::Rose));
        mounted.done();
        mounted.done();

        assert_eq!(mounted.applied().len(), 1);
        assert_eq!(mounted.dismissed(), 0);
    }

    #[test]
    fn reset_restores_the_draft_without_applying_it() {
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_accent(AccentColor::Rose));
        assert!(mounted.read(|sheet| sheet.is_dirty()));

        let sheet = mounted.sheet.clone();
        sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.reset(cx);
        });
        mounted.test.refresh().expect("the sheet repainted");

        assert!(!mounted.read(|sheet| sheet.is_dirty()));
        assert_eq!(
            mounted.read(|sheet| sheet.draft()),
            SettingsDraft::from_theme(&global::current())
        );
        assert!(mounted.applied().is_empty(), "reset applied something");
    }

    #[test]
    fn abandoning_the_sheet_restores_the_previewed_appearance() {
        let theme = Theme::new(
            AppearanceMode::Dark,
            BaseTone::Slate,
            AccentColor::Blue,
            TextSizePreset::Standard,
        );
        let _guard = ThemeGuard::install(theme);

        let mut mounted = mount();
        mounted.choose(|sheet| {
            sheet.set_tone(BaseTone::Stone);
            sheet.set_accent(AccentColor::Rose);
            sheet.set_text_size(TextSizePreset::Large);
        });

        mounted.press("escape");

        assert!(mounted.applied().is_empty());
        assert_eq!(mounted.dismissed(), 1);
        assert_eq!(
            global::current(),
            theme,
            "the theme moved without Done being pressed"
        );
    }

    #[test]
    fn cancelling_a_preview_preserves_a_new_system_appearance() {
        let theme = Theme::default().with_appearance(AppearanceMode::System);
        let _guard = ThemeGuard::install(theme);
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_tone(BaseTone::Stone));
        global::set_system_appearance(!theme.system_appearance);
        mounted.press("escape");
        let mut expected = theme;
        expected.system_appearance = !theme.system_appearance;
        assert_eq!(global::current(), expected);
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn escape_backs_out_once() {
        let mut mounted = mount();
        mounted.press("escape");
        mounted.press("escape");
        assert_eq!(mounted.dismissed(), 1);
    }

    #[test]
    fn return_applies_and_a_second_return_does_not_apply_twice() {
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.set_accent(AccentColor::Orange));
        mounted.press("enter");
        mounted.press("enter");

        assert_eq!(mounted.applied().len(), 1);
        assert_eq!(mounted.dismissed(), 0);
    }

    #[test]
    fn a_key_the_sheet_does_not_use_is_left_alone() {
        // Stopping propagation on every key would strand the reader: Tab would
        // stop moving focus while the sheet had it.
        let mut mounted = mount();
        mounted.press("down");

        assert_eq!(mounted.dismissed(), 0);
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn a_click_on_the_backdrop_backs_out() {
        let mut mounted = mount();
        // A corner, which no part of the sheet can be over.
        mounted.click_at(gpui::point(px(4.0), px(4.0)));

        assert_eq!(mounted.dismissed(), 1);
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn a_click_on_the_sheet_itself_does_not_back_out() {
        let mut mounted = mount();
        // Dead centre of the window, which is inside the sheet. Reaching for a
        // control must not read as backing out.
        mounted.click_at(gpui::point(px(960.0), px(540.0)));

        assert_eq!(mounted.dismissed(), 0);
    }

    #[test]
    fn a_re_check_is_asked_for_rather_than_run_here() {
        // Discovery is async and can shell out to a login shell, so the sheet
        // cannot run it from a click handler.
        let mut mounted = mount();
        assert_eq!(mounted.rechecked(), 0);

        mounted.recheck();

        assert_eq!(mounted.rechecked(), 1);
        assert!(mounted.applied().is_empty());
        assert_eq!(mounted.dismissed(), 0);
    }

    #[test]
    fn rows_delivered_after_the_sheet_opens_replace_what_it_shows() {
        let mut mounted = mount();
        assert!(mounted.read(|sheet| sheet.rows().is_empty()));

        let temp = tempfile::tempdir().expect("a temp dir");
        let codex = spec_at(&temp.path().join("no-codex"));
        let not_found = BinaryHealth::NotFound;
        let reports = [ProviderReport::new(&codex, &not_found)];
        let sheet = mounted.sheet.clone();
        sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.set_health(cx, &reports);
        });
        mounted.test.refresh().expect("the sheet repainted");

        let rows = mounted.read(|sheet| sheet.rows().to_vec());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tone, RowTone::Idle);
    }

    #[test]
    fn runtime_paths_keep_readable_width_in_a_narrow_settings_window() {
        let mut mounted = mount();
        mounted.choose(|sheet| {
            sheet.page = SettingsPage::Providers;
            sheet.rows = vec![
                HealthRow {
                    id: "codex".into(),
                    title: "Codex".into(),
                    tone: RowTone::Ok,
                    badge: "Ready".into(),
                    detail: "Codex is installed.".into(),
                    path: Some("/Users/example/.codex/packages/standalone/releases/0.155.1-aarch64-apple-darwin/bin/codex".into()),
                },
                HealthRow {
                    id: "claude".into(),
                    title: "Claude Code".into(),
                    tone: RowTone::Ok,
                    badge: "Ready".into(),
                    detail: "Claude Code is installed.".into(),
                    path: Some("/Users/example/.local/share/claude/versions/2.1.283".into()),
                },
            ];
        });
        let mut visual = VisualTestContext::from_window(mounted.window, &mounted.test);
        visual.simulate_resize(gpui::size(px(1_000.0), px(640.0)));
        mounted
            .test
            .refresh()
            .expect("the narrow settings sheet painted");
        for selector in ["provider-path-codex", "provider-path-claude"] {
            let bounds = visual
                .debug_bounds(selector)
                .expect("runtime path measured");
            assert!(
                bounds.size.width > px(100.0),
                "{selector} collapsed to {bounds:?}"
            );
        }
    }

    #[test]
    fn runtime_path_labels_preserve_the_install_root_and_executable() {
        assert_eq!(
            runtime_path_label(
                "/Users/example/.codex/packages/standalone/releases/0.155.1-aarch64-apple-darwin/bin/codex",
                Some(Path::new("/Users/example")),
            ),
            "~/.codex/…/bin/codex",
        );
        assert_eq!(
            runtime_path_label("/opt/homebrew/bin/codex", None),
            "/opt/homebrew/bin/codex",
        );
        assert_eq!(
            runtime_path_label(
                "/Users/example2/bin/codex",
                Some(Path::new("/Users/example"))
            ),
            "/Users/example2/bin/codex",
            "a different home must not be abbreviated",
        );
    }

    #[test]
    fn runtime_path_labels_bound_long_unicode_filenames_without_splitting_them() {
        let path = format!("/Users/example/.local/{}-codex", "工具".repeat(30));
        let label = runtime_path_label(&path, Some(Path::new("/Users/example")));
        assert!(label.chars().count() <= 42);
        assert!(label.starts_with("~/.local/"));
        assert!(label.contains('…'));
        assert!(label.ends_with("-codex"));
    }

    #[test]
    fn the_navigation_groups_hold_every_page_exactly_once() {
        let grouped: Vec<_> = SettingsPage::GROUPS
            .iter()
            .flat_map(|(_, pages)| pages.iter().copied())
            .collect();
        for page in SettingsPage::ALL {
            assert_eq!(
                grouped.iter().filter(|grouped| **grouped == page).count(),
                1,
                "{page:?}"
            );
        }
        assert_eq!(grouped.len(), SettingsPage::ALL.len());
    }

    #[test]
    fn a_search_finds_pages_by_what_they_hold_not_only_their_name() {
        assert!(SettingsPage::Appearance.matches("accent"));
        assert!(SettingsPage::ChatDefaults.matches("reasoning"));
        assert!(SettingsPage::Providers.matches("quarantine"));
        assert!(!SettingsPage::Shortcuts.matches("accent"));
        // Every command a shortcut names finds the page that lists it.
        for shortcut in crate::commands::key_bindings() {
            assert!(
                SettingsPage::Shortcuts.matches(&shortcut.label.to_lowercase()),
                "{}",
                shortcut.label
            );
        }
        for page in SettingsPage::ALL {
            assert!(page.matches(""), "an empty search hides {page:?}");
        }
    }

    #[test]
    fn slash_searches_return_opens_the_first_match_and_escape_clears_first() {
        let mut mounted = mount();
        mounted.press("/");
        let search = mounted.read(|sheet| sheet.search.clone());
        let searching = || {
            let search = search.clone();
            move |window: &mut Window, cx: &mut App| {
                search.read(cx).focus_handle().is_focused(window)
            }
        };
        let focused = mounted
            .window
            .update(&mut mounted.test, {
                let searching = searching();
                move |_, window, cx| searching(window, cx)
            })
            .unwrap();
        assert!(focused, "slash moves the keys to the search");

        mounted.test.simulate_input(mounted.window, "short");
        mounted.test.refresh().unwrap();
        assert_eq!(
            mounted
                .test
                .read(|cx| mounted.sheet.read(cx).visible_pages(cx)),
            vec![SettingsPage::Shortcuts]
        );

        // The first Escape empties the search; the sheet stays open.
        mounted.press("escape");
        assert_eq!(mounted.dismissed(), 0);
        assert_eq!(
            mounted.test.read(|cx| search.read(cx).value().to_owned()),
            ""
        );

        mounted.test.simulate_input(mounted.window, "accent");
        mounted.press("enter");
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::Appearance);
        assert!(mounted.applied().is_empty(), "return in the search applied");
        let focused = mounted
            .window
            .update(&mut mounted.test, {
                let searching = searching();
                move |_, window, cx| searching(window, cx)
            })
            .unwrap();
        assert!(!focused, "opening a match hands the keys back to the sheet");

        // Nothing matching leaves the page where it was.
        mounted.press("/");
        mounted.test.simulate_input(mounted.window, "zzz");
        mounted.press("enter");
        assert_eq!(mounted.read(|sheet| sheet.page), SettingsPage::Appearance);
        assert!(mounted.applied().is_empty());
    }

    #[test]
    fn the_shortcuts_page_lists_every_key_the_app_binds() {
        assert!(
            SettingsPage::ALL.contains(&SettingsPage::Shortcuts),
            "reachable from the navigation like any other page"
        );
        let mut mounted = mount();
        mounted.choose(|sheet| sheet.page = SettingsPage::Shortcuts);
        // `refresh` queues the repaint; the next update is what draws it.
        mounted.test.update(|_| {});
        let mut visual = VisualTestContext::from_window(mounted.window, &mounted.test);
        let table = crate::commands::key_bindings();
        assert!(table.len() > 20, "the table, not a sample of it");
        for shortcut in table {
            // `debug_bounds` wants a `'static` selector; a test can afford one.
            let selector: &'static str =
                Box::leak(format!("shortcut:{}", shortcut.symbols()).into_boxed_str());
            assert!(
                visual.debug_bounds(selector).is_some(),
                "{} ({}) is bound but not listed",
                shortcut.label,
                shortcut.symbols()
            );
        }
    }

    #[test]
    fn the_sheet_paints_every_state_it_can_be_in() {
        // The cheapest end-to-end check there is: the element tree has to
        // compose for all four row tones, an empty provider list, a settled
        // sheet, and a backdrop click.
        let temp = tempfile::tempdir().expect("a temp dir");
        let shadow = temp.path().join("shadow");
        std::fs::create_dir(&shadow).expect("the folder was created");

        let ok_spec = spec_at(&temp.path().join("ok"));
        let quarantined_spec = spec_at(&temp.path().join("q"));
        let bad_spec = spec_at(&shadow);
        let absent_spec = spec_at(&temp.path().join("absent"));
        let checking_spec = spec_at(&temp.path().join("checking"));

        let mut mounted = mount();
        let sheet = mounted.sheet.clone();
        let ok = available("/bin/tool");
        let quarantined = BinaryHealth::Quarantined {
            path: "/Users/someone/.local/bin/tool".into(),
        };
        let not_found = BinaryHealth::NotFound;
        let checking = BinaryHealth::Checking;
        // A bundled path: the copy for it takes a different branch.
        let bundled = BinaryHealth::Quarantined {
            path: "/Applications/Tool.app/Contents/MacOS/tool".into(),
        };
        let reports = [
            ProviderReport::new(&ok_spec, &ok),
            ProviderReport::new(&quarantined_spec, &quarantined),
            ProviderReport::new(&bad_spec, &not_found),
            ProviderReport::new(&absent_spec, &not_found),
            ProviderReport::new(&checking_spec, &checking),
            ProviderReport::new(&ok_spec, &bundled),
        ];
        sheet.update(&mut mounted.test, |sheet, cx| {
            sheet.page = SettingsPage::Providers;
            sheet.set_health(cx, &reports);
        });
        mounted
            .test
            .refresh()
            .expect("the sheet painted every tone");

        mounted.choose(|sheet| {
            sheet.set_tone(BaseTone::Neutral);
            sheet.set_appearance(AppearanceMode::Light);
            sheet.set_accent(AccentColor::Emerald);
            sheet.set_text_size(TextSizePreset::Comfortable);
        });
        mounted.click_at(gpui::point(px(960.0), px(540.0)));
        mounted.done();
    }
}
