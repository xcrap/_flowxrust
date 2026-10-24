//! The sheet that turns a typed folder path into a project.
//!
//! The modal behind "Add project" accepts a typed path or a folder chosen in
//! the native directory picker. Both go through the same validation and leave
//! the final Add action to the reader.
//!
//! # Two halves, deliberately separated
//!
//! [`validate_project_path`] takes a string and touches the filesystem, and
//! nothing else. Every rule about what counts as a project lives there, so all
//! of them are testable without mounting a window.
//!
//! [`ProjectPicker`] owns only what a view needs: the field, the last verdict,
//! and two callbacks. It owns no application state, which is why the same sheet
//! can be opened from a menu row, a sidebar button, or a test without any of
//! them knowing about the others.
//!
//! # Why the resolved path is reported, not the typed one
//!
//! `on_add` always receives the canonicalised path. A symlink is a name, not a
//! location, and the reader can repoint one between choosing a project and the
//! next launch; persisting the link would let that happen silently. This is the
//! same rule `fx-core`'s runtime discovery follows before it executes a binary.

use std::path::{Path, PathBuf};

use fx_design::components::surface::banner;
use fx_design::components::{
    BadgeTone, ButtonStyle, IconButtonProps, badge, button, divider, icon_button, panel,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, IconSize, Layout, Shadow, Spacing, TextRole, Theme, global};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, MouseButton, Render, SharedString,
    Subscription, Window, div, px,
};

use crate::text_field::TextField;

/// Why a typed path cannot become a project.
///
/// Every variant carries the path it is about, because the sheet shows these
/// strings verbatim and "that path" is the only thing a reader can act on.
#[derive(Debug, thiserror::Error)]
pub enum PickerError {
    #[error("Enter the path to a folder.")]
    Empty,
    #[error("{path} is not a full path. Start it with /.")]
    NotAbsolute { path: PathBuf },
    #[error("There is nothing at {path}.")]
    DoesNotExist { path: PathBuf },
    #[error("{path} is a file. FlowX needs the folder that holds the code.")]
    NotADirectory { path: PathBuf },
    #[error("FlowX cannot read {path}. Check its permissions.")]
    NotReadable { path: PathBuf },
    /// A resolved path that is not valid UTF-8.
    ///
    /// Unreachable on macOS: APFS rejects a non-UTF-8 name outright, so no
    /// `canonicalize` can return one. It stays because `on_add` hands over a
    /// `String`, and the day the picker runs somewhere that allows such a name
    /// the alternative is silently substituting `U+FFFD` for a byte the reader
    /// never typed. It has no test for the same reason.
    #[error("{path} has a name FlowX cannot show. Rename it, or choose another folder.")]
    NotAUtf8Path { path: PathBuf },
    #[error("Could not check {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A folder that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCandidate {
    /// The canonicalised path, never the text that was typed.
    pub path: PathBuf,
    /// The last component of `path`, or `path` itself when it has none.
    pub name: String,
    /// Whether a `.git` entry sits directly inside.
    ///
    /// Advisory. A directory without one is a perfectly good project — plenty
    /// of them are a Cargo workspace whose repository sits one level up — so
    /// this is reported, never enforced.
    pub is_git_repo: bool,
}

/// Check a typed path and describe the folder it names.
///
/// The checks run in the order of the ways this is wrong, and each one earns
/// its place:
///
/// 1. Empty input is not a bad path, it is no input.
/// 2. A relative path is refused even when it resolves, because the process
///    working directory is not the reader's idea of where they are. An app
///    launched from Finder inherits `/` from launchd, so a path that works in
///    the terminal it was tested in can point somewhere else entirely here.
/// 3. Symlinks are resolved *before* anything is reported back, so the caller
///    is told about a directory rather than about a name for one.
/// 4. The name is read from the resolved path, not from the typed one.
/// 5. Existence, then kind, then readability — "that is a file" sends the
///    reader somewhere useful, where "there is nothing there" would not.
pub fn validate_project_path(raw: &str) -> Result<ProjectCandidate, PickerError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(PickerError::Empty);
    }

    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(PickerError::NotAbsolute { path });
    }

    let resolved = std::fs::canonicalize(&path).map_err(|error| io_error(&path, error))?;

    let Some(resolved_text) = resolved.to_str() else {
        return Err(PickerError::NotAUtf8Path { path: resolved });
    };

    let metadata = std::fs::metadata(&resolved).map_err(|error| io_error(&resolved, error))?;
    if !metadata.is_dir() {
        return Err(PickerError::NotADirectory { path: resolved });
    }

    // Open and read the directory rather than inspecting its mode bits. The
    // bits say what was asked for; only the read says what this process can
    // actually do, which is the question the reader is asking.
    read_one_entry(&resolved)?;

    Ok(ProjectCandidate {
        name: display_name(&resolved, resolved_text),
        is_git_repo: resolved.join(".git").exists(),
        path: resolved,
    })
}

/// A failure against a path we have already resolved.
fn io_error(path: &Path, error: std::io::Error) -> PickerError {
    match error.kind() {
        std::io::ErrorKind::NotFound => PickerError::DoesNotExist {
            path: path.to_path_buf(),
        },
        std::io::ErrorKind::PermissionDenied => PickerError::NotReadable {
            path: path.to_path_buf(),
        },
        _ => PickerError::Io {
            path: path.to_path_buf(),
            source: error,
        },
    }
}

/// Read one entry out of the directory, or report why it could not be read.
///
/// Consuming an entry rather than only opening the handle is deliberate: a
/// directory that opens but whose contents cannot be listed is as unreadable
/// as one that will not open, and only the read tells the two apart.
fn read_one_entry(path: &Path) -> Result<(), PickerError> {
    let mut entries = std::fs::read_dir(path).map_err(|error| io_error(path, error))?;
    if let Some(entry) = entries.next() {
        entry.map_err(|error| io_error(path, error))?;
    }
    Ok(())
}

/// The last component of `path`, falling back to the path itself.
///
/// `/` has no last component, and a row labelled with nothing is worse than a
/// row labelled `/`.
fn display_name(path: &Path, path_text: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(path_text)
        .to_string()
}

// ── The sheet ──────────────────────────────────────────────────────────────

/// Called with the *resolved* project path.
type AddHandler = Box<dyn Fn(String, &mut Window, &mut App)>;
/// Called when the reader backs out without choosing anything.
type CancelHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// The project picker sheet, mounted over the window by its caller.
///
/// The picker decides only whether a path is usable, and reports the outcome;
/// adding the project and unmounting the sheet are the caller's business.
/// That is why both outcomes are callbacks rather than state here.
pub struct ProjectPicker {
    field: Entity<TextField>,
    on_add: AddHandler,
    on_cancel: CancelHandler,
    /// The text the current verdict was computed from.
    ///
    /// The field notifies on every caret blink as well as on every keystroke,
    /// so re-validating unconditionally means two filesystem round-trips a
    /// second for as long as the field has focus. Comparing the text first
    /// makes the cost proportional to typing instead.
    checked: String,
    /// The last verdict, or `None` while the field holds nothing to judge.
    verdict: Option<Result<ProjectCandidate, PickerError>>,
    browsing: bool,
    browse_error: Option<String>,
    /// Set once either callback has fired.
    ///
    /// Without it a Return that lands just before the sheet is unmounted adds
    /// the project a second time, and the caller reacts to the first callback,
    /// not the second.
    settled: bool,
    /// Resolved folders of the projects already in the sidebar.
    ///
    /// Choosing one of these focuses that project rather than adding it again,
    /// and the sheet says so before the reader commits to it.
    open_roots: Vec<PathBuf>,
    /// Keeps the field observation alive; dropping it stops re-validation.
    _observation: Option<Subscription>,
}

impl ProjectPicker {
    /// Build a sheet with the two outcomes it reports.
    ///
    /// The caller mounts the returned entity and unmounts it when either
    /// callback fires. [`Self::open`] is the usual entry point, because it also
    /// puts the caret in the field.
    pub fn new(cx: &mut App, on_add: AddHandler, on_cancel: CancelHandler) -> Entity<Self> {
        cx.new(|cx| {
            let field = cx.new(|cx| {
                TextField::new(cx)
                    .placeholder("/Users/you/code/project")
                    .max_len(1024)
            });

            // Built before the struct so the observation can name it, and
            // installed after so the struct owns it: a `Subscription` that is
            // dropped cancels the observation, and a sheet that stopped
            // re-validating would look exactly like one that had nothing to
            // report.
            let mut picker = Self {
                field,
                on_add,
                on_cancel,
                checked: String::new(),
                verdict: None,
                browsing: false,
                browse_error: None,
                settled: false,
                open_roots: Vec::new(),
                _observation: None,
            };
            picker._observation = Some(cx.observe(
                &picker.field,
                |picker: &mut Self, _field: Entity<TextField>, cx: &mut Context<Self>| {
                    picker.revalidate(cx)
                },
            ));
            picker
        })
    }

    /// Build a sheet and put the caret in its field.
    ///
    /// Focus rather than wait for a click: the sheet is one text field, so a
    /// reader asked to aim at it before typing has already been asked for one
    /// thing too many.
    pub fn open(
        window: &mut Window,
        cx: &mut App,
        on_add: AddHandler,
        on_cancel: CancelHandler,
    ) -> Entity<Self> {
        let picker = Self::new(cx, on_add, on_cancel);
        picker.read(cx).field.read(cx).focus_handle().focus(window);
        picker
    }

    /// The focus handle for the path field.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field.read(cx).focus_handle()
    }

    /// Whether "Add project" would do anything right now.
    ///
    /// A method rather than a stored flag: the verdict changes on every
    /// keystroke, and a second copy of it is a second thing to be wrong.
    pub fn can_add(&self) -> bool {
        matches!(self.verdict, Some(Ok(_)))
    }

    /// Tell the sheet which folders are already open, as resolved paths.
    pub fn set_open_roots(&mut self, roots: Vec<PathBuf>) {
        self.open_roots = roots;
    }

    /// Whether the checked folder is already a project in the sidebar.
    pub fn is_already_open(&self) -> bool {
        matches!(&self.verdict, Some(Ok(candidate)) if self.open_roots.contains(&candidate.path))
    }

    /// Re-check the field after it changed.
    ///
    /// The filesystem is touched here rather than in `render` on purpose: a
    /// render runs on every frame and often for reasons that have nothing to
    /// do with the text, and a stat per frame is not a cost worth paying.
    fn revalidate(&mut self, cx: &mut Context<Self>) {
        let raw = self.field.read(cx).value().to_string();
        if raw == self.checked {
            return;
        }
        self.browse_error = None;
        self.checked = raw;
        self.verdict = if self.checked.trim().is_empty() {
            None
        } else {
            Some(validate_project_path(&self.checked))
        };
        cx.notify();
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        if self.browsing || self.settled {
            return;
        }
        self.browsing = true;
        self.browse_error = None;
        let response = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose project folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = response
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |picker, cx| picker.finish_browse(result, cx));
        })
        .detach();
        cx.notify();
    }

    fn finish_browse(
        &mut self,
        result: Result<Option<Vec<PathBuf>>, String>,
        cx: &mut Context<Self>,
    ) {
        self.browsing = false;
        if self.settled {
            return;
        }
        match result {
            Ok(Some(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    if let Some(path) = path.to_str() {
                        self.field.update(cx, |field, cx| field.set_value(path, cx));
                        self.revalidate(cx);
                    } else {
                        self.browse_error =
                            Some("This folder has a name FlowX cannot display.".into());
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.browse_error = Some(format!("Could not open the folder picker: {error}"))
            }
        }
        cx.notify();
    }

    /// Add the checked path. Does nothing unless one currently validates.
    fn confirm(&mut self, window: &mut Window, cx: &mut App) {
        if self.settled {
            return;
        }
        let Some(Ok(candidate)) = &self.verdict else {
            return;
        };
        self.settled = true;
        // Lossless: `validate_project_path` refuses any path that is not UTF-8,
        // so this cannot substitute a replacement character for a byte.
        let path = candidate.path.to_string_lossy().into_owned();
        (self.on_add)(path, window, cx);
    }

    /// Back out without choosing anything.
    fn cancel(&mut self, window: &mut Window, cx: &mut App) {
        if self.settled {
            return;
        }
        self.settled = true;
        (self.on_cancel)(window, cx);
    }
}

impl Render for ProjectPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();

        if self.settled {
            // Draw nothing rather than a dead sheet. The caller owns the
            // unmount, and until it happens an inert sheet that still looks
            // live is one more click away from adding the project twice.
            return div().w(px(0.0)).h(px(0.0)).into_any_element();
        }

        let backdrop = div()
            .absolute()
            .size_full()
            // `overlay`, not `overlay_light`: this is the token layer's
            // strongest wash, and a scrim over the whole window is the
            // strongest thing it is asked to do.
            .bg(Colors::overlay(&theme))
            .on_mouse_down(MouseButton::Left, {
                let this = this.clone();
                move |_event, window, cx| {
                    this.update(cx, |picker, cx| {
                        picker.cancel(window, cx);
                        cx.notify();
                    });
                }
            });

        let sheet = div()
            .w(px_len(Layout::DIALOG_WIDTH))
            .max_w_full()
            // Padding around the panel so the shadow has somewhere to land: a
            // styled element's shadow is clipped to the element's own bounds,
            // which is why the menus take an edge instead.
            .p(px_len(Spacing::SM))
            .fx_shadow(Shadow::dialog(Colors::overlay(&theme)))
            // Swallow the click that lands on the sheet's own padding, so
            // reaching for the field does not read as reaching past the sheet.
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(panel(
                self.body(
                    &theme,
                    &this,
                    (window.viewport_size().height - px(Spacing::HUGE * 4.0))
                        .max(px(Spacing::HUGE * 2.0)),
                ),
            ));

        div()
            .id("project-picker-overlay")
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
                    // Return is captured rather than bubbled because the field
                    // claims `enter` itself — that is how a single-line field
                    // submits — and stops propagation on the way up. The
                    // capture phase runs the other way, so this is the only
                    // place that both sees the key and still holds a window.
                    .capture_key_down({
                        let this = this.clone();
                        move |event: &KeyDownEvent, window, cx| {
                            let cancel = event.keystroke.key == "escape";
                            let confirm = event.keystroke.key == "enter" && this.read(cx).can_add();
                            if !cancel && !confirm {
                                return;
                            }
                            cx.stop_propagation();
                            this.update(cx, |picker, cx| {
                                if cancel {
                                    picker.cancel(window, cx);
                                } else {
                                    picker.confirm(window, cx);
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

impl ProjectPicker {
    /// The panel's contents: what is being asked for, the field, the verdict,
    /// and the two ways out.
    fn body(
        &self,
        theme: &Theme,
        this: &Entity<ProjectPicker>,
        max_body_height: gpui::Pixels,
    ) -> impl IntoElement {
        let for_cancel = this.clone();
        let for_add = this.clone();
        let for_close = this.clone();
        let for_browse = this.clone();

        div()
            .w_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px_len(Spacing::MD))
                    .px(px_len(Spacing::XXL))
                    .py(px_len(Spacing::XL))
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px_len(Spacing::XXS))
                            .child(div().fx_title2().child("Add project"))
                            .child(
                                div()
                                    .fx_caption()
                                    .text_color(Colors::fg_secondary(theme))
                                    .child("Choose a local folder for this project."),
                            ),
                    )
                    .child(icon_button(
                        IconButtonProps::new(Icon::XMARK, "Close project picker"),
                        move |_, window, cx| {
                            for_close.update(cx, |picker, cx| {
                                picker.cancel(window, cx);
                                cx.notify();
                            });
                        },
                    )),
            )
            .child(divider())
            .child(
                div()
                    .id("project-picker-content")
                    .w_full()
                    .min_h_0()
                    .max_h(max_body_height)
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px_len(Spacing::MD))
                            .p(px_len(Spacing::XXL))
                            .child(div().fx_body_medium().child("Folder path"))
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap(px_len(Spacing::SM))
                                    .child(div().flex_1().min_w_0().child(self.field.clone()))
                                    .child(button(
                                        "Browse…",
                                        ButtonStyle::Secondary,
                                        !self.browsing,
                                        move |_, _, cx| for_browse.update(cx, |picker, cx| picker.browse(cx)),
                                    )),
                            )
                            .child(
                                div()
                                    .fx_caption()
                                    .text_color(Colors::fg_secondary(theme))
                                    .child("Browse for a folder or enter its full path. Git repositories are detected automatically."),
                            )
                            .when_some(self.browse_error.as_ref(), |body, error| {
                                body.child(banner(
                                    Colors::error_muted(theme),
                                    Colors::fg(theme),
                                    div().fx_caption().child(SharedString::from(error.clone())),
                                ))
                            })
                            .child(self.verdict_row(theme)),
                    ),
            )
            .child(divider())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px_len(Spacing::SM))
                    .px(px_len(Spacing::XXL))
                    .py(px_len(Spacing::LG))
                    .flex_shrink_0()
                    .child(button(
                        "Cancel",
                        ButtonStyle::Secondary,
                        true,
                        move |_event, window, cx| {
                            for_cancel.update(cx, |picker, cx| {
                                picker.cancel(window, cx);
                                cx.notify();
                            });
                        },
                    ))
                    .child(button(
                        if self.is_already_open() {
                            "Show project"
                        } else {
                            "Add project"
                        },
                        ButtonStyle::Primary,
                        self.can_add(),
                        move |_event, window, cx| {
                            for_add.update(cx, |picker, cx| {
                                picker.confirm(window, cx);
                                cx.notify();
                            });
                        },
                    )),
            )
    }

    /// The line under the field: why the path will not do, or what will be
    /// opened once it does.
    ///
    /// The resolved path is shown next to the name rather than the typed text,
    /// because those two can differ — that is the whole point of resolving it,
    /// and a reader who cannot see the difference has not been told anything.
    fn verdict_row(&self, theme: &Theme) -> AnyElement {
        match &self.verdict {
            Some(Ok(candidate)) => candidate_row(candidate, self.is_already_open(), theme),
            Some(Err(error)) => banner(
                Colors::error_muted(theme),
                // `fg` rather than `error`: the invariant is that `fg` clears
                // 4.5:1 on every `*_muted` fill, and the tint already carries
                // the tone.
                Colors::fg(theme),
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px_len(Spacing::SM))
                    .child(Icon::EXCLAMATIONMARK_TRIANGLE_FILL.render(
                        IconSize::Small,
                        Colors::error(theme),
                        theme,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .fx_caption()
                            .child(SharedString::from(error.to_string())),
                    ),
            )
            .into_any_element(),
            // Nothing typed, so nothing to say. The space is not reserved: a
            // sheet that grows a line the moment it opens moves the field out
            // from under the caret.
            None => div().w(px(0.0)).h(px(0.0)).into_any_element(),
        }
    }
}

/// What will be opened once the path is accepted.
///
/// The resolved path is shown next to the name rather than the text that was
/// typed, because those two can differ — that is the whole point of resolving
/// it, and a reader who cannot see the difference has not been told anything.
fn candidate_row(candidate: &ProjectCandidate, already_open: bool, theme: &Theme) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::XXXS))
                .child(
                    div()
                        .fx_body_medium()
                        .truncate()
                        .child(SharedString::from(candidate.name.clone())),
                )
                .child(
                    div()
                        .fx_role(TextRole::MonoSmall)
                        .text_color(Colors::fg_secondary(theme))
                        .truncate()
                        .child(SharedString::from(
                            candidate.path.to_string_lossy().into_owned(),
                        )),
                ),
        );

    // Advisory, so it is a badge rather than a requirement stated in prose: a
    // folder without one of these is still a folder the reader may well mean.
    if candidate.is_git_repo {
        row = row.child(badge(
            SharedString::from("Git repository"),
            BadgeTone::Success,
            true,
        ));
    }
    // Said before the reader confirms, so "Show project" is not a surprise.
    if already_open {
        row = row.child(badge(
            SharedString::from("Already open"),
            BadgeTone::Info,
            true,
        ));
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AnyWindowHandle, Keystroke, Modifiers, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;

    // ── Pure validation ──────────────────────────────────────────────────

    /// A canonicalised path, because `tempfile` hands back a symlinked one.
    ///
    /// `/var` and `/tmp` are symlinks on macOS, so the directory a test creates
    /// is not the path `canonicalize` reports. Comparing against the raw
    /// tempdir path would make every assertion in this module a lie.
    fn resolved(path: &Path) -> PathBuf {
        path.canonicalize().expect("the path resolves")
    }

    fn validate(path: &Path) -> Result<ProjectCandidate, PickerError> {
        validate_project_path(&path.to_string_lossy())
    }

    #[test]
    fn nothing_typed_is_not_a_bad_path() {
        // The distinction matters: an empty field is not yet an answer, so
        // reporting it as a rejected path would put an error under a box the
        // reader has not filled in.
        assert!(matches!(validate_project_path(""), Err(PickerError::Empty)));
        assert!(matches!(
            validate_project_path("   \t\n "),
            Err(PickerError::Empty)
        ));
    }

    #[test]
    fn a_relative_path_is_refused_even_when_it_exists() {
        // The test process runs in the crate directory, so this one really does
        // resolve from here. It would work if the reader typed it into a
        // terminal; it must not work here, because an app launched from Finder
        // inherits `/` from launchd and the two disagree about where "here" is.
        assert!(
            Path::new("Cargo.toml").exists(),
            "the premise of this test is that the relative path resolves"
        );
        assert!(matches!(
            validate_project_path("Cargo.toml"),
            Err(PickerError::NotAbsolute { .. })
        ));
        assert!(matches!(
            validate_project_path("src/"),
            Err(PickerError::NotAbsolute { .. })
        ));
    }

    #[test]
    fn a_path_that_is_not_there_says_so() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let missing = temp.path().join("not-here");
        assert!(matches!(
            validate(&missing),
            Err(PickerError::DoesNotExist { .. })
        ));
    }

    #[test]
    fn a_file_is_reported_as_a_file_and_not_as_nothing() {
        // "There is nothing at /x" and "/x is a file" send the reader to two
        // different places, and only one of them exists.
        let temp = tempfile::tempdir().expect("a temp dir");
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").expect("the file was written");

        match validate(&file) {
            Err(PickerError::NotADirectory { path }) => assert_eq!(path, resolved(&file)),
            other => panic!("expected NotADirectory, got {other:?}"),
        }
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_refused() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let locked = temp.path().join("locked");
        std::fs::create_dir(&locked).expect("the directory was created");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("the mode was set");

        let outcome = validate(&locked);

        // Restored before the assertion, so the temp dir can be cleaned up even
        // if the assertion fails.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("the mode was restored");

        match outcome {
            Err(PickerError::NotReadable { path }) => assert_eq!(path, resolved(&locked)),
            other => panic!("expected NotReadable, got {other:?}"),
        }
    }

    #[test]
    fn a_symlink_reports_its_target_not_its_own_name() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let target = temp.path().join("real-project");
        std::fs::create_dir(&target).expect("the target was created");
        let link = temp.path().join("shortcut");
        std::os::unix::fs::symlink(&target, &link).expect("the link was created");

        let candidate = validate(&link).expect("the link resolves to a folder");
        assert_eq!(candidate.path, resolved(&target));
        assert_eq!(candidate.name, "real-project");
    }

    #[test]
    fn a_git_repository_is_reported() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("with-git");
        std::fs::create_dir(&project).expect("the project was created");
        std::fs::create_dir(project.join(".git")).expect("the repository was created");

        assert!(validate(&project).expect("it validates").is_git_repo);
    }

    #[test]
    fn a_folder_without_a_repository_is_still_a_project() {
        // The rule that keeps the flag advisory: refusing here would rule out
        // every workspace whose repository sits one level up.
        let temp = tempfile::tempdir().expect("a temp dir");
        let plain = temp.path().join("just-a-folder");
        std::fs::create_dir(&plain).expect("the directory was created");

        let candidate = validate(&plain).expect("it validates");
        assert!(!candidate.is_git_repo);
        assert_eq!(candidate.name, "just-a-folder");
    }

    #[test]
    fn a_name_with_spaces_and_other_scripts_survives() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("проект ✨ files");
        std::fs::create_dir(&project).expect("the directory was created");

        let candidate = validate(&project).expect("it validates");
        assert_eq!(candidate.name, "проект ✨ files");
        assert_eq!(candidate.path, resolved(&project));
    }

    #[test]
    fn a_trailing_slash_names_the_same_folder() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("the directory was created");

        let with_slash = format!("{}/", project.to_string_lossy());
        let candidate = validate_project_path(&with_slash).expect("it validates");
        assert_eq!(candidate.path, resolved(&project));
        assert_eq!(candidate.name, "project");
    }

    #[test]
    fn a_folder_with_no_last_component_is_named_after_itself() {
        // `/` has no file name, and a row with a blank label is worse than a
        // row labelled with the one path that has no components.
        let candidate = validate_project_path("/").expect("the root is a folder");
        assert_eq!(candidate.path, PathBuf::from("/"));
        assert_eq!(candidate.name, "/");
    }

    #[test]
    fn every_error_names_the_path_it_is_about() {
        // The sheet shows these strings verbatim, and "it" is not actionable.
        let temp = tempfile::tempdir().expect("a temp dir");
        let missing = temp.path().join("gone");
        let message = match validate(&missing) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a missing path should not validate"),
        };
        assert!(
            message.contains(&missing.to_string_lossy().into_owned()),
            "{message} does not name the path"
        );
    }

    // ── The sheet ───────────────────────────────────────────────────────

    /// One-line view whose only job is to give the sheet a `Render` stack.
    ///
    /// The field registers a platform input handler while it draws, and that
    /// reaches for the view currently painting, which only a `Render` impl
    /// populates — the same reason the transcript tests mount a probe.
    struct Host {
        picker: Entity<ProjectPicker>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.picker.clone().into_any_element()
        }
    }

    struct Mounted {
        test: TestAppContext,
        window: AnyWindowHandle,
        picker: Entity<ProjectPicker>,
        added: Rc<RefCell<Vec<String>>>,
        cancelled: Rc<RefCell<usize>>,
    }

    impl Mounted {
        /// Type into the field the way the platform does, and let the verdict
        /// settle. Each view test gets its own store-free context, and they run
        /// concurrently, so nothing here is shared.
        fn type_into(&mut self, text: &str) {
            self.test.update(|cx| {
                self.picker.update(cx, |picker, cx| {
                    picker
                        .field
                        .update(cx, |field, cx| field.set_value(text.to_string(), cx));
                });
            });
            self.test.refresh().expect("the sheet repainted");
        }

        fn can_add(&self) -> bool {
            self.test.read(|cx| self.picker.read(cx).can_add())
        }

        /// The message the sheet would be showing, if any.
        fn shown_error(&self) -> Option<String> {
            self.test.read(|cx| match &self.picker.read(cx).verdict {
                Some(Err(error)) => Some(error.to_string()),
                _ => None,
            })
        }

        fn added(&self) -> Vec<String> {
            self.added.borrow().clone()
        }

        fn cancelled(&self) -> usize {
            *self.cancelled.borrow()
        }

        /// Send a key to the window, as the platform would.
        fn press(&mut self, key: &str) {
            self.test
                .dispatch_keystroke(self.window, Keystroke::parse(key).expect("a parsable key"));
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
        }
    }

    fn mount() -> Mounted {
        let mut test = TestAppContext::single();
        let added: Rc<RefCell<Vec<String>>> = Rc::default();
        let cancelled: Rc<RefCell<usize>> = Rc::default();

        let picker = test.update(|cx| {
            let for_add = added.clone();
            let for_cancel = cancelled.clone();
            ProjectPicker::new(
                cx,
                Box::new(move |path, _window, _cx| {
                    for_add.borrow_mut().push(path);
                }),
                Box::new(move |_window, _cx| {
                    *for_cancel.borrow_mut() += 1;
                }),
            )
        });

        let mounted = picker.clone();
        let window = test.add_window(move |window, cx| {
            // Focused here rather than in `open` because a mount that skipped
            // it would never deliver a keystroke to the sheet, and a sheet that
            // cannot be typed into is not the thing under test.
            mounted.update(cx, |picker, cx| {
                picker.field.read(cx).focus_handle().focus(window);
            });
            Host { picker: mounted }
        });

        test.refresh().expect("the sheet painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");

        Mounted {
            window: window.into(),
            test,
            picker,
            added,
            cancelled,
        }
    }

    #[test]
    fn an_untouched_sheet_shows_no_error_and_offers_nothing() {
        // The complaint belongs under a field the reader has filled in.
        let mounted = mount();
        assert_eq!(mounted.shown_error(), None);
        assert!(!mounted.can_add());
    }

    #[test]
    fn a_sheet_with_a_bad_path_says_why_and_refuses_to_add() {
        let mut mounted = mount();
        mounted.type_into("/definitely/not/a/real/folder");

        assert!(
            mounted
                .shown_error()
                .is_some_and(|message| message.contains("/definitely/not/a/real/folder"))
        );
        assert!(!mounted.can_add());
    }

    #[test]
    fn a_sheet_with_a_good_path_offers_to_add_it() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("the project was created");
        std::fs::create_dir(project.join(".git")).expect("the repository was created");

        let mut mounted = mount();
        mounted.type_into(&project.to_string_lossy());

        assert!(mounted.can_add());
        assert_eq!(mounted.shown_error(), None);
    }

    #[test]
    fn a_folder_that_is_already_open_is_recognised_through_a_symlink() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("project");
        let other = temp.path().join("other");
        std::fs::create_dir(&project).expect("the project was created");
        std::fs::create_dir(&other).expect("the other folder was created");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&project, &link).expect("the link was created");

        let mut mounted = mount();
        mounted.test.update(|cx| {
            mounted.picker.update(cx, |picker, _| {
                picker.set_open_roots(vec![resolved(&project)])
            });
        });
        mounted.type_into(&link.to_string_lossy());
        assert!(
            mounted
                .test
                .read(|cx| mounted.picker.read(cx).is_already_open())
        );
        // Still a valid choice: confirming it focuses the open project.
        assert!(mounted.can_add());
        mounted.type_into(&other.to_string_lossy());
        assert!(
            !mounted
                .test
                .read(|cx| mounted.picker.read(cx).is_already_open())
        );
    }

    #[test]
    fn browsing_selects_and_validates_a_folder_without_adding_it() {
        let temp = tempfile::tempdir().expect("a project folder");
        let mut mounted = mount();
        mounted.picker.update(&mut mounted.test, |picker, cx| {
            picker.browsing = true;
            picker.finish_browse(Ok(Some(vec![temp.path().to_path_buf()])), cx);
            assert!(!picker.browsing);
            assert_eq!(picker.field.read(cx).value(), temp.path().to_str().unwrap());
        });
        mounted.test.refresh().unwrap();
        assert!(mounted.can_add());
        assert_eq!(mounted.shown_error(), None);
        assert!(
            mounted.added().is_empty(),
            "Browse must preserve Add confirmation"
        );
        mounted.press("enter");
        assert_eq!(
            mounted.added(),
            vec![resolved(temp.path()).to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn cancelling_native_browse_preserves_the_typed_path_and_validation() {
        let temp = tempfile::tempdir().expect("a project folder");
        let mut mounted = mount();
        mounted.type_into(temp.path().to_str().unwrap());
        mounted.picker.update(&mut mounted.test, |picker, cx| {
            picker.browsing = true;
            picker.finish_browse(Ok(None), cx);
            assert!(!picker.browsing);
            assert_eq!(picker.field.read(cx).value(), temp.path().to_str().unwrap());
        });
        assert!(mounted.can_add());
        assert_eq!(mounted.shown_error(), None);
        assert!(mounted.added().is_empty());
        assert_eq!(
            mounted.cancelled(),
            0,
            "canceling Browse must leave Add project open"
        );
    }

    #[test]
    fn a_late_native_choice_cannot_change_a_closed_project_dialog() {
        let mut mounted = mount();
        mounted.type_into("/existing-draft");
        mounted.press("escape");
        mounted.picker.update(&mut mounted.test, |picker, cx| {
            picker.finish_browse(Ok(Some(vec![PathBuf::from("/new-choice")])), cx);
            assert_eq!(picker.field.read(cx).value(), "/existing-draft");
        });
        assert_eq!(mounted.cancelled(), 1);
        assert!(mounted.added().is_empty());
    }

    #[test]
    fn clearing_the_field_clears_the_error() {
        let mut mounted = mount();
        mounted.type_into("/definitely/not/a/real/folder");
        assert!(mounted.shown_error().is_some());

        mounted.type_into("");
        assert_eq!(mounted.shown_error(), None);
        assert!(!mounted.can_add());
    }

    #[test]
    fn return_adds_the_resolved_path() {
        let temp = tempfile::tempdir().expect("a temp dir");
        let target = temp.path().join("real-project");
        std::fs::create_dir(&target).expect("the target was created");
        let link = temp.path().join("shortcut");
        std::os::unix::fs::symlink(&target, &link).expect("the link was created");

        let mut mounted = mount();
        mounted.type_into(&link.to_string_lossy());
        mounted.press("enter");

        // The resolved path, not the link: a link is a name, and the reader can
        // repoint it between choosing a project and the next launch.
        assert_eq!(
            mounted.added(),
            vec![resolved(&target).to_string_lossy().into_owned()]
        );
        assert_eq!(mounted.cancelled(), 0);
    }

    #[test]
    fn return_on_a_rejected_path_does_nothing() {
        let mut mounted = mount();
        mounted.type_into("/definitely/not/a/real/folder");
        mounted.press("enter");

        assert!(mounted.added().is_empty());
        assert_eq!(mounted.cancelled(), 0);
    }

    #[test]
    fn escape_backs_out() {
        let mut mounted = mount();
        mounted.press("escape");

        assert_eq!(mounted.cancelled(), 1);
        assert!(mounted.added().is_empty());
    }

    #[test]
    fn a_second_escape_does_not_back_out_twice() {
        let mut mounted = mount();
        mounted.press("escape");
        mounted.press("escape");

        assert_eq!(mounted.cancelled(), 1);
    }

    #[test]
    fn a_second_return_does_not_add_twice() {
        // The caller unmounts on the first callback, so the second has to be
        // refused by the sheet rather than by timing.
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("the directory was created");

        let mut mounted = mount();
        mounted.type_into(&project.to_string_lossy());
        mounted.press("enter");
        mounted.press("enter");

        assert_eq!(mounted.added().len(), 1);
        assert!(mounted.test.read(|cx| mounted.picker.read(cx).settled));
    }

    #[test]
    fn a_click_on_the_backdrop_backs_out() {
        let mut mounted = mount();
        // A corner, which no part of the sheet can be over: the sheet is
        // centred and 440pt wide.
        mounted.click_at(gpui::point(px(4.0), px(4.0)));

        assert_eq!(mounted.cancelled(), 1);
        assert!(mounted.added().is_empty());
    }

    #[test]
    fn a_click_on_the_sheet_itself_does_not_back_out() {
        let mut mounted = mount();
        // Dead centre of the window, which is inside the sheet: it is 440pt
        // wide and centred. Reaching for the field must not read as reaching
        // past the sheet.
        mounted.click_at(gpui::point(px(960.0), px(540.0)));

        assert_eq!(mounted.cancelled(), 0);
    }

    #[test]
    fn the_sheet_paints_every_state_it_can_be_in() {
        // The cheapest end-to-end check there is: the element tree has to
        // compose for an untouched sheet, a rejected one, and an accepted one,
        // and each of those takes a different branch through `verdict_row`.
        let temp = tempfile::tempdir().expect("a temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("the project was created");
        std::fs::create_dir(project.join(".git")).expect("the repository was created");
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").expect("the file was written");

        let mut mounted = mount();
        mounted.type_into("/definitely/not/a/real/folder");
        mounted.type_into(&file.to_string_lossy());
        mounted.type_into(&project.to_string_lossy());
        mounted.press("enter");
    }
}
