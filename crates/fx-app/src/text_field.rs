//! A text field: FlowX's first way to accept typed text.
//!
//! GPUI 0.2.2 ships no text input. IME input arrives through
//! `Window::handle_input`, which may only be called during the *paint* phase and
//! needs the field's window-space bounds — which only exist once layout has run.
//! A `div` can supply neither from `render`, so the field is wrapped in a small
//! custom [`Element`] whose job is to shape the text, register the platform
//! handler against the element's real bounds, and draw the selection and caret
//! from that one shaping.
//!
//! # Three coordinate systems, one choke point
//!
//! Everything the platform hands us is in **UTF-16** offsets; the value is a
//! Rust `String` in **UTF-8 bytes**; and drawing happens in **window pixels**.
//! Conversions exist so no method has to hold two of them at once, and every
//! mutation — typed, pasted, deleted, or assigned through
//! [`TextField::set_value`] — goes through [`TextField::edit`]. That is what
//! makes `max_len` and the single-line newline rule true regardless of where
//! the text came from.
//!
//! # Which keys this field claims
//!
//! Editing keystrokes are handled here and stop propagation, which also stops
//! AppKit from inserting the same character a second time. Printable characters
//! are deliberately *not* claimed: they fall through to `NSInputContext`, so dead
//! keys and IME composition keep working the way the platform intends. Return is
//! claimed even in a multi-line field, for the same reason.
//!
//! The modifiers follow AppKit rather than inventing their own: Option reaches a
//! word and Cmd reaches the edge of the line, for movement and deletion alike.
//!
//! # Undo
//!
//! User edits are recorded in groups the way an `NSTextView` groups them: a run
//! of typing is one step, a run of deleting is another, and moving the caret
//! ends the run. Cut and paste are always a step of their own. A composition is
//! never split: everything the IME does between its first marked text and its
//! commit joins the group it began in, so undo cannot land on half a character.
//! Assigning a value with [`TextField::set_value`] is not an edit and starts the
//! history afresh — undoing past a sent prompt into the previous draft would
//! resurrect text that has already gone.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fx_design::styled::{FxRefinement, px_len};
use fx_design::theme::{BorderWidth, Colors, Radii, Spacing, TextRole, Theme, Typography, global};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Background, Bounds, ClipboardItem, ContentMask, Context, CursorStyle,
    DispatchPhase, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Font, FontFeatures, FontStyle, GlobalElementId, InspectorElementId, IntoElement,
    KeyDownEvent, LayoutId, LineLayout, Modifiers, MouseButton, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Render, ShapedLine, SharedString, Task, TextAlign, TextRun, Timer,
    UTF16Selection, Window, WrappedLine, div, fill, point, px, size,
};

gpui::actions!(
    text_field,
    [
        /// Undo the focused field's last group of edits.
        Undo,
        /// Redo what the last undo took back.
        Redo,
        /// Cut the selection to the clipboard.
        Cut,
        /// Copy the selection to the clipboard.
        Copy,
        /// Paste the clipboard over the selection.
        Paste,
        /// Select the whole value.
        SelectAll
    ]
);

// ── Metrics ────────────────────────────────────────────────────────────────
//
// Everything here is a token except the caret's own width and its blink period:
// the caret is not a border and the design system has no animation preset for
// it. The width reuses the focused-field border weight so the two read as one
// decision rather than two unrelated numbers.

/// Horizontal padding inside the field.
const FIELD_PADDING_X: f32 = Spacing::SM;
/// Vertical padding inside the field.
const FIELD_PADDING_Y: f32 = Spacing::XXS;
/// Width of the blinking caret.
const CARET_WIDTH: f32 = BorderWidth::REGULAR;
/// Thickness of the underline under text the IME is still composing.
const MARKED_UNDERLINE: f32 = BorderWidth::REGULAR;
/// One half of a blink cycle: long enough not to read as a flicker, short
/// enough that the caret still reads as present.
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(530);

/// Everything about a field's own geometry and type, resolved from the theme.
#[derive(Clone)]
struct FieldMetrics {
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    /// Total inset from the element's bounds to the text: padding plus border,
    /// because GPUI lays the border inside the box.
    inset_x: Pixels,
    inset_y: Pixels,
    /// Whether the text wraps, which is the same thing as "is multi-line".
    wrap: bool,
    /// Ceiling on visible rows; `1.0` is a single line.
    max_visible_rows: f32,
    /// The border weight this frame's focus state calls for.
    border_width: f32,
}

/// The submit callback, boxed because a field is moved around by value and a
/// bare `dyn Fn` would make every one of them larger than the state it guards.
type SubmitHandler = Box<dyn Fn(String, &mut Context<TextField>)>;

/// What only the paint phase can know.
///
/// The wrapper element writes these while it draws, and the platform's input
/// callbacks read them much later, when no view is on the stack, so they cannot
/// live on the view. A cell rather than a field because the writer is the
/// element, not the view.
#[derive(Default)]
struct FieldGeometry {
    /// The field's bounds, in window coordinates.
    bounds: Option<Bounds<Pixels>>,
    /// The measured width of the text area. The next render needs it to wrap and
    /// scroll correctly, because this render is what produces the measurement.
    content_width: f32,
    #[cfg(test)]
    first_painted_text_origin: Option<Point<Pixels>>,
}

/// The field state the element needs while it draws, snapshotted so painting
/// never has to re-enter the view.
struct FieldSnapshot {
    selection: Selection,
    caret_on: bool,
    focused: bool,
    enabled: bool,
    dragging: bool,
}

/// A single-line or multi-line text field.
///
/// Mount it with `cx.new(TextField::new)` and draw it with
/// `.into_any_element()`. The public API below is the whole surface; everything
/// under it exists to make the platform's text input work.
pub struct TextField {
    value: String,
    selection: Selection,
    /// The IME's in-progress composition, in bytes of `value`. While it is set
    /// GPUI routes keys to the input context first, so Return confirms the
    /// composition instead of submitting or inserting a newline.
    marked: Option<Range<usize>>,
    placeholder: String,
    focus_handle: FocusHandle,
    on_submit: Option<SubmitHandler>,
    multiline: Option<f32>,
    enabled: bool,
    max_len: usize,
    secret: bool,
    bare: bool,
    /// The caret is drawn only on alternate phases while the field has focus, so
    /// this is the phase rather than a "should I draw" flag.
    caret_on: bool,
    /// Whether the field holds focus, cached from render because the blink task
    /// has no window to ask.
    focused: bool,
    /// A pointer drag is in progress. Held so the field keeps tracking moves that
    /// leave its bounds, which is the whole of drag-to-select.
    dragging: bool,
    /// Held so the blink task is not detached.
    blink: Option<Task<()>>,
    blinking: bool,
    geometry: Rc<RefCell<FieldGeometry>>,
    history: UndoHistory,
}

impl TextField {
    /// An empty, enabled, single-line field.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            value: String::new(),
            selection: Selection::collapsed(0),
            marked: None,
            placeholder: String::new(),
            focus_handle: cx.focus_handle(),
            on_submit: None,
            multiline: None,
            enabled: true,
            max_len: 0,
            secret: false,
            bare: false,
            caret_on: true,
            focused: false,
            dragging: false,
            blink: None,
            blinking: false,
            geometry: Rc::default(),
            history: UndoHistory::default(),
        }
    }

    /// The current text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Use the surrounding composer surface instead of drawing another input box.
    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    /// Keep secrets out of painted glyphs and clipboard actions while retaining the original answer.
    pub fn secret(mut self, secret: bool) -> Self {
        self.secret = secret;
        self
    }

    pub fn is_secret(&self) -> bool {
        self.secret
    }

    fn display_value(&self) -> Cow<'_, str> {
        if self.secret {
            Cow::Owned("•".repeat(self.value.chars().count()))
        } else {
            Cow::Borrowed(&self.value)
        }
    }

    fn display_index(&self, source_index: usize) -> usize {
        if self.secret {
            self.value[..snap_back(&self.value, source_index)]
                .chars()
                .count()
                * '•'.len_utf8()
        } else {
            source_index
        }
    }

    fn source_index(&self, display_index: usize) -> usize {
        if self.secret {
            self.value
                .char_indices()
                .nth(display_index / '•'.len_utf8())
                .map_or(self.value.len(), |(index, _)| index)
        } else {
            snap_back(&self.value, display_index)
        }
    }

    /// Replace the text; used to restore a draft or clear after submit.
    ///
    /// Goes through the same path as typing, so `max_len` and the single-line
    /// newline rule hold here too — restoring a draft cannot smuggle in a
    /// newline or exceed the cap.
    ///
    /// Not an edit, so it is not undoable: the history starts again from here.
    pub fn set_value(&mut self, value: impl Into<String>, cx: &mut Context<Self>) {
        let caret = self.value.len();
        self.edit(0..caret, &value.into());
        self.selection = Selection::collapsed(self.selection.head);
        self.history = UndoHistory::default();
        cx.notify();
    }

    /// Focus handle, so a caller can move focus to this field.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Placeholder shown while empty.
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = text.into();
        self
    }

    /// Called when the user presses Return (single-line) or Cmd/Ctrl+Return
    /// (multi-line).
    pub fn on_submit(mut self, submit: impl Fn(String, &mut Context<Self>) + 'static) -> Self {
        self.on_submit = Some(Box::new(submit));
        self
    }

    /// Attach a submit callback after restoring a field from a saved draft.
    pub fn set_on_submit(&mut self, submit: impl Fn(String, &mut Context<Self>) + 'static) {
        self.on_submit = Some(Box::new(submit));
    }

    /// Allow wrapping and a cap on visible height.
    pub fn multiline(mut self, max_lines: f32) -> Self {
        self.multiline = Some(max_lines.max(1.0));
        self
    }

    /// Whether the field refuses input.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Maximum length, counted in characters rather than bytes; 0 is unlimited.
    ///
    /// Characters, not bytes: a field capped at 10 has room for ten emoji, and a
    /// byte cap would cut one in half.
    pub fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    // ── Editing ────────────────────────────────────────────────────────────

    /// The one place the value changes.
    ///
    /// Every rule that constrains what may live in a field lives here rather
    /// than in each caller, so a single-line field can never gain a newline and
    /// `max_len` can never split a character, whatever the source.
    fn edit(&mut self, range: Range<usize>, insert: &str) {
        if !self.enabled {
            return;
        }
        // Any edit ends a composition; `replace_and_mark_text_in_range` marks
        // its own insertion again afterwards.
        self.marked = None;
        let range = sanitize_range(&self.value, range);
        let insert = if self.multiline.is_some() {
            insert.to_string()
        } else {
            // Stripping rather than rejecting keeps a paste with a trailing
            // newline useful, and a lone Return does nothing at all.
            insert
                .chars()
                .filter(|c| *c != '\n' && *c != '\r')
                .collect()
        };
        let insert = truncate_to_max_len(&self.value, &range, &insert, self.max_len);

        let head = range.start + insert.len();
        self.value.replace_range(range, &insert);
        self.selection = Selection::collapsed(head);
        self.caret_on = true;
        self.dragging = false;
    }

    /// An edit the user made, recorded for undo.
    ///
    /// Everything a person does to the value comes through here; [`Self::edit`]
    /// stays the primitive so that assigning a value is not mistaken for one.
    /// An edit that changes nothing records nothing, so Backspace at the start
    /// of the field does not leave an empty step to undo.
    fn user_edit(&mut self, kind: EditKind, range: Range<usize>, insert: &str) {
        if !self.enabled {
            return;
        }
        // Read before `edit` clears it: the input method's own next step in a
        // composition belongs to the group its first marked text opened. A cut
        // or paste is never that step. It ends the composition and is a step of
        // its own; joined, Undo took the paste back along with the composition.
        let composing = self.marked.is_some() && kind != EditKind::Clipboard;
        let before = self.snapshot();
        self.edit(range, insert);
        if self.value == before.value {
            return;
        }
        if !composing {
            self.history.will_edit(kind, before);
        }
        self.history.did_edit(kind, self.selection);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            value: self.value.clone(),
            selection: self.selection,
        }
    }

    /// Put back a recorded state. It passed every rule when it was recorded,
    /// so it is assigned directly rather than edited in.
    fn restore(&mut self, snapshot: Snapshot) {
        self.value = snapshot.value;
        self.selection = Selection {
            anchor: snap_back(&self.value, snapshot.selection.anchor),
            head: snap_back(&self.value, snapshot.selection.head),
        };
        self.marked = None;
        self.caret_on = true;
        self.dragging = false;
    }

    /// Take back the last group of edits. Returns whether there was one.
    ///
    /// Refused while the IME is composing: the composition is not finished,
    /// so neither is the group it belongs to, and pulling the text out from
    /// under the input method would leave it editing a range that has gone.
    pub fn undo(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.enabled || self.marked.is_some() {
            return false;
        }
        let current = self.snapshot();
        let Some(previous) = self.history.undo(current) else {
            return false;
        };
        self.restore(previous);
        cx.notify();
        true
    }

    /// Reapply what the last undo took back. Returns whether there was any.
    pub fn redo(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.enabled || self.marked.is_some() {
            return false;
        }
        let current = self.snapshot();
        let Some(next) = self.history.redo(current) else {
            return false;
        };
        self.restore(next);
        cx.notify();
        true
    }

    /// Whether there is anything to undo, for a caller that greys out a control.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether there is anything to redo.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Where text from the platform lands: the range it names, else the
    /// composition it replaces, else the selection. A dead key or an IME sends
    /// its final text with no range, meaning "over what is being composed".
    fn input_range(&self, range_utf16: Option<Range<usize>>) -> Range<usize> {
        let range = range_utf16
            .map(|range| utf16_range(&self.value, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.range());
        sanitize_range(&self.value, range)
    }

    /// Insert final text from the platform, ending any composition.
    fn commit_text(&mut self, range_utf16: Option<Range<usize>>, text: &str) {
        let range = self.input_range(range_utf16);
        self.user_edit(EditKind::Typing, range, text);
    }

    /// Replace the composition with the IME's in-progress text and keep it
    /// marked, with the selection where the IME puts it inside that text.
    /// Empty text cancels the composition.
    fn mark_text(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
    ) {
        if !self.enabled {
            return;
        }
        let range = self.input_range(range_utf16);
        self.user_edit(EditKind::Typing, range.clone(), text);
        // `edit` may have stripped or truncated the text; the caret is where
        // what actually went in ends.
        let marked = range.start..self.selection.head;
        if marked.is_empty() {
            return;
        }
        if let Some(selected) = selected_utf16 {
            let composed = &self.value[marked.clone()];
            self.selection = Selection {
                anchor: marked.start
                    + byte_index_for_utf16(composed, selected.start.min(selected.end)),
                head: marked.start
                    + byte_index_for_utf16(composed, selected.end.max(selected.start)),
            };
        }
        self.marked = Some(marked);
    }

    /// The text the selection covers, or `""` when nothing is selected.
    fn selected_text(&self) -> String {
        self.value
            .get(self.selection.range())
            .map(str::to_string)
            .unwrap_or_default()
    }

    // ── Keys ───────────────────────────────────────────────────────────────

    /// Handle the editing keys. Returns whether the field claimed the key.
    ///
    /// Separate from the GPUI callback so the whole table can be exercised
    /// without a window. Everything here works on the value alone — including
    /// vertical movement, which steps by the value's logical lines rather than
    /// its visual rows — so none of it needs a shaped layout. For the same
    /// reason a line edge is a logical line's edge: Cmd-Right in a wrapped
    /// paragraph goes to the end of the paragraph, not of the visual row.
    fn handle_edit_key(
        &mut self,
        key: &str,
        extend: bool,
        reach: Reach,
        cx: &mut Context<Self>,
    ) -> bool {
        let range = self.selection.range();
        let caret = self.selection.head;
        match key {
            "enter" => {
                if self.multiline.is_some() && reach != Reach::Line {
                    self.user_edit(EditKind::Typing, range, "\n");
                } else if let Some(submit) = self.on_submit.as_ref() {
                    submit(self.value.clone(), cx);
                }
                true
            }
            "backspace" => {
                let range = if range.is_empty() {
                    let from = match reach {
                        Reach::Character => prev_boundary(&self.value, caret),
                        Reach::Word => prev_word_boundary(&self.value, caret),
                        // Already at the start of the line, Cmd-Backspace joins
                        // it to the one above, as AppKit's does.
                        Reach::Line => match move_to_line_edge(&self.value, caret, false) {
                            start if start == caret => prev_boundary(&self.value, caret),
                            start => start,
                        },
                    };
                    from..caret
                } else {
                    range
                };
                self.user_edit(EditKind::DeleteBackward, range, "");
                true
            }
            "delete" => {
                let range = if range.is_empty() {
                    let to = match reach {
                        Reach::Character => next_boundary(&self.value, caret),
                        Reach::Word => next_word_boundary(&self.value, caret),
                        Reach::Line => match move_to_line_edge(&self.value, caret, true) {
                            end if end == caret => next_boundary(&self.value, caret),
                            end => end,
                        },
                    };
                    caret..to
                } else {
                    range
                };
                self.user_edit(EditKind::DeleteForward, range, "");
                true
            }
            "left" | "right" => {
                let forward = key == "right";
                self.selection = match reach {
                    Reach::Line => move_to_edge(self.selection, extend, forward, |at| {
                        move_to_line_edge(&self.value, at, forward)
                    }),
                    reach => move_horizontal(
                        &self.value,
                        self.selection,
                        forward,
                        extend,
                        reach == Reach::Word,
                    ),
                };
                self.moved_caret();
                true
            }
            "up" | "down" => {
                let forward = key == "down";
                let moved = match reach {
                    // Cmd-Up and Cmd-Down go to either end of the whole value.
                    Reach::Line => {
                        let end = if forward { self.value.len() } else { 0 };
                        move_to_edge(self.selection, extend, forward, |_| end)
                    }
                    _ => move_vertical(&self.value, self.selection, forward, extend),
                };
                if moved == self.selection {
                    // Nothing to move: let the key reach whatever scrolls behind
                    // the field instead of swallowing it.
                    return false;
                }
                self.selection = moved;
                self.moved_caret();
                true
            }
            "home" | "end" => {
                let edge = move_to_line_edge(&self.value, self.selection.head, key == "end");
                self.selection = self.selection.with_head(edge, extend);
                self.moved_caret();
                true
            }
            _ => false,
        }
    }

    /// The caret moved without an edit, which ends the run of typing that an
    /// undo would otherwise take back together with whatever is typed next.
    fn moved_caret(&mut self) {
        self.caret_on = true;
        self.history.break_group();
    }

    /// Select-all, copy, cut, and the paste fallback.
    ///
    /// Each of these is also an action (see the `actions!` at the top), which is
    /// how the Edit menu reaches the focused field. When the app binds the keys,
    /// the binding fires first and this never sees them; it remains so that a
    /// field still works in a window that binds nothing.
    ///
    /// Paste lives here *and* in the platform handler. macOS routes Cmd-V through
    /// `NSInputContext` first, so when the system handles it this never runs;
    /// when the system does not, this is the only path that would paste.
    fn handle_clipboard_key(&mut self, key: &str, by_word: bool, cx: &mut Context<Self>) -> bool {
        if !by_word {
            return false;
        }
        match key.to_ascii_lowercase().as_str() {
            "a" => self.select_all(),
            "c" => self.copy(cx),
            "x" => self.cut(cx),
            "v" => self.paste(cx),
            _ => return false,
        }
        true
    }

    /// Cmd-Z and Shift-Cmd-Z, the unbound fallback for [`Undo`] and [`Redo`].
    ///
    /// Claimed whether or not there is anything to undo, so the key never falls
    /// through to a shortcut behind the field.
    fn handle_history_key(
        &mut self,
        key: &str,
        command: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if !command || !key.eq_ignore_ascii_case("z") {
            return false;
        }
        if shift {
            self.redo(cx);
        } else {
            self.undo(cx);
        }
        true
    }

    fn select_all(&mut self) {
        self.selection = Selection::select_all(self.value.len());
        self.moved_caret();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if self.secret {
            return;
        }
        let text = self.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if self.secret {
            return;
        }
        let text = self.selected_text();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.user_edit(EditKind::Clipboard, self.selection.range(), "");
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.user_edit(EditKind::Clipboard, self.selection.range(), &text);
        }
    }

    /// One of the Edit menu's commands, as the focused field receives it.
    ///
    /// Claimed even when it does nothing: an action that bubbled past the field
    /// would reach the window, which has no better idea of what to undo.
    fn edit_command(&mut self, command: EditCommand, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        match command {
            EditCommand::Undo => {
                self.undo(cx);
            }
            EditCommand::Redo => {
                self.redo(cx);
            }
            EditCommand::Cut => self.cut(cx),
            EditCommand::Copy => self.copy(cx),
            EditCommand::Paste => self.paste(cx),
            EditCommand::SelectAll => self.select_all(),
        }
        cx.notify();
    }

    // ── Caret placement ────────────────────────────────────────────────────

    /// The byte offset a click at `point` should put the caret at.
    ///
    /// Degrades rather than failing: a point outside the text clamps to the
    /// nearest row and column, because a click in the padding is still a click
    /// on the field.
    fn index_at(&self, at: Point<Pixels>, window: &Window) -> usize {
        let Some(bounds) = self.geometry.borrow().bounds else {
            return 0;
        };
        let theme = global::current();
        let metrics = self.metrics(&theme, self.focused);
        let content = content_bounds(&bounds, &metrics);
        // Shaped with the colour the field is drawn in, not an arbitrary one: the
        // colour is part of the text system's cache key, so anything else would
        // reshape the line on every click.
        let color = if self.enabled {
            Colors::fg(&theme)
        } else {
            Colors::fg_tertiary(&theme)
        };
        let layout = build_layout(
            window,
            &metrics,
            &self.display_value(),
            content.size.width,
            self.display_index(self.selection.head),
            color,
            None,
        );
        self.source_index(layout.index_at(point(at.x - content.origin.x, at.y - content.origin.y)))
    }

    fn metrics(&self, theme: &Theme, focused: bool) -> FieldMetrics {
        let style = Typography::style(TextRole::Body, theme);
        // A focused field uses the focused border weight from the token layer, so
        // the caret and the focus ring read as one decision.
        let border_width = if self.bare {
            0.0
        } else if focused {
            BorderWidth::REGULAR
        } else {
            BorderWidth::HAIRLINE
        };
        FieldMetrics {
            font: Font {
                family: SharedString::from(style.family),
                features: FontFeatures::default(),
                weight: style.weight(),
                style: FontStyle::default(),
                fallbacks: None,
            },
            font_size: px(style.size),
            line_height: px(style.size * TextRole::Body.line_height_multiplier()),
            inset_x: px(if self.bare {
                0.0
            } else {
                FIELD_PADDING_X + border_width
            }),
            inset_y: px(if self.bare {
                Spacing::XXS
            } else {
                FIELD_PADDING_Y + border_width
            }),
            wrap: self.multiline.is_some(),
            max_visible_rows: self.multiline.unwrap_or(1.0),
            border_width,
        }
    }

    /// Keep the caret blinking for as long as the field has focus.
    ///
    /// Driven by a timer rather than by redrawing every frame: a caret that
    /// repaints at display rate looks identical and costs sixty frames a second
    /// for the privilege. The loop's only exit conditions are read back out of
    /// the view, because the task has no window of its own.
    fn ensure_blink(&mut self, cx: &mut Context<Self>) {
        if self.blinking {
            return;
        }
        self.blinking = true;
        let this = cx.weak_entity();
        self.blink = Some(cx.spawn(async move |_this, cx| {
            loop {
                Timer::after(CARET_BLINK_INTERVAL).await;
                let keep_going = this
                    .update(cx, |field, cx| {
                        if !field.focused || !field.enabled {
                            field.blinking = false;
                            return false;
                        }
                        field.caret_on = !field.caret_on;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    fn mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled {
            // Suppresses the div's own "clicking focuses me" behaviour, which
            // would put a caret in a field that cannot be typed into.
            window.prevent_default();
            return;
        }
        let index = self.index_at(event.position, window);
        let clicked = match event.click_count {
            2 => {
                let range = word_range(&self.value, index);
                Selection {
                    anchor: range.start,
                    head: range.end,
                }
            }
            count if count >= 3 => Selection {
                anchor: move_to_line_edge(&self.value, index, false),
                head: move_to_line_edge(&self.value, index, true),
            },
            _ => Selection::collapsed(index),
        };
        // Clicking away keeps whatever was composed, as typed text. A marked
        // range left behind would point at text the caret has already left.
        self.marked = None;
        // A plain click collapses; shift-click extends from the existing anchor.
        self.selection = if event.modifiers.shift {
            clicked.with_anchor(self.selection.anchor)
        } else {
            clicked
        };
        self.dragging = true;
        self.moved_caret();
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        let keystroke = &event.keystroke;
        let modifiers: Modifiers = keystroke.modifiers;
        // `secondary` is Cmd on macOS and Ctrl elsewhere, so one spelling of
        // "the shortcut modifier" is right on every platform.
        let command = modifiers.secondary();
        let handled = self.handle_clipboard_key(&keystroke.key, command, cx)
            || self.handle_history_key(&keystroke.key, command, modifiers.shift, cx)
            || self.handle_edit_key(
                &keystroke.key,
                modifiers.shift,
                Reach::from_modifiers(modifiers),
                cx,
            );
        if handled {
            // Stopping propagation is also what stops AppKit inserting the same
            // character again on the way down.
            cx.stop_propagation();
            cx.notify();
        }
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let focused = self.focus_handle.is_focused(window);
        self.focused = focused;
        // The caret is only a phase while the field has focus; unfocusing must
        // not leave it frozen mid-blink when the field is focused again.
        self.caret_on = self.caret_on && focused;
        if focused {
            self.ensure_blink(cx);
        }

        let metrics = self.metrics(&theme, focused);
        // The measured width is last frame's: this render produces the
        // measurement, so a first frame simply has nothing to wrap against yet.
        let visible_width = px(self.geometry.borrow().content_width);
        let text_color = if self.enabled {
            Colors::fg(&theme)
        } else {
            Colors::fg_tertiary(&theme)
        };
        let layout = build_layout(
            window,
            &metrics,
            &self.display_value(),
            visible_width,
            self.display_index(self.selection.head),
            text_color,
            // A secret field draws bullets, whose offsets are not the value's.
            self.marked.clone().filter(|_| !self.secret),
        );
        let visible_rows = layout
            .rows
            .len()
            .clamp(1, metrics.max_visible_rows.ceil() as usize);
        let content_height = metrics.line_height * visible_rows as f32;

        // A placeholder is shaped on its own rather than through the layout,
        // because it is not part of the value: no rows, no caret, no selection.
        let placeholder = if self.value.is_empty() && !self.placeholder.is_empty() {
            Some(ShapedText::Single(Box::new(shape_one_line(
                window,
                &self.placeholder,
                &metrics,
                Colors::fg_tertiary(&theme),
            ))))
        } else {
            None
        };

        let snapshot = FieldSnapshot {
            selection: Selection {
                anchor: self.display_index(self.selection.anchor),
                head: self.display_index(self.selection.head),
            },
            caret_on: self.caret_on,
            focused,
            enabled: self.enabled,
            dragging: self.dragging,
        };

        let border_color = match (focused, self.enabled) {
            (true, true) => Colors::accent(&theme),
            (true, false) => Colors::border(&theme),
            (false, true) => Colors::border_subtle(&theme),
            (false, false) => Colors::border_subtle(&theme),
        };
        let hover_color = Colors::border(&theme);

        // GPUI lays the border inside the box, so the height the div is given is
        // the text area plus its padding plus its border. Getting this wrong by
        // half a point would clip the bottom of the last line.
        let outer_height = f32::from(content_height + metrics.inset_y * 2.0);
        let element = div()
            .flex()
            .w_full()
            .min_w_0()
            .px(metrics.inset_x - px(metrics.border_width))
            .py(metrics.inset_y - px(metrics.border_width))
            .h(px_len(outer_height))
            .when(!self.bare, |element| {
                element
                    .rounded(px_len(Radii::SM))
                    .bg(if self.enabled {
                        Colors::input_bg(&theme)
                    } else {
                        Colors::bg(&theme)
                    })
                    .border(px_len(metrics.border_width))
                    .border_color(border_color)
            })
            .cursor(if self.enabled {
                CursorStyle::IBeam
            } else {
                CursorStyle::Arrow
            })
            .track_focus(&self.focus_handle)
            .when(!self.bare, |element| {
                element.hover(move |style| {
                    if focused {
                        style
                    } else {
                        style.fx_border_color(hover_color)
                    }
                })
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|field, event, window, cx| field.mouse_down(event, window, cx)),
            )
            .on_key_down(cx.listener(|field, event, _window, cx| field.key_down(event, cx)))
            // The Edit menu dispatches to whatever holds focus, so these are
            // what make its items reach this field — and what enable them.
            .on_action(
                cx.listener(|field, _: &Undo, _, cx| field.edit_command(EditCommand::Undo, cx)),
            )
            .on_action(
                cx.listener(|field, _: &Redo, _, cx| field.edit_command(EditCommand::Redo, cx)),
            )
            .on_action(
                cx.listener(|field, _: &Cut, _, cx| field.edit_command(EditCommand::Cut, cx)),
            )
            .on_action(
                cx.listener(|field, _: &Copy, _, cx| field.edit_command(EditCommand::Copy, cx)),
            )
            .on_action(
                cx.listener(|field, _: &Paste, _, cx| field.edit_command(EditCommand::Paste, cx)),
            )
            .on_action(cx.listener(|field, _: &SelectAll, _, cx| {
                field.edit_command(EditCommand::SelectAll, cx)
            }));

        TextFieldElement {
            child: element.into_any_element(),
            field: cx.entity(),
            focus: self.focus_handle.clone(),
            geometry: self.geometry.clone(),
            metrics,
            snapshot,
            layout,
            placeholder,
        }
    }
}

// ── The wrapper element ────────────────────────────────────────────────────

/// Wraps the field's element tree so the platform input handler can be
/// registered during paint.
///
/// `Window::handle_input` asserts the paint phase and needs the element's
/// window-space bounds, and neither is available from `render`. This element does
/// nothing except delegate, and then — once it knows where it is — register the
/// handler and draw the parts of the field that depend on the shaped text.
struct TextFieldElement {
    child: AnyElement,
    field: Entity<TextField>,
    focus: FocusHandle,
    geometry: Rc<RefCell<FieldGeometry>>,
    metrics: FieldMetrics,
    snapshot: FieldSnapshot,
    layout: TextLayout,
    placeholder: Option<ShapedText>,
}

impl Element for TextFieldElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        // Neither this element nor the div inside it needs an id: nothing here
        // uses `on_click`, `active`, or a tooltip, and an id would only be a
        // second thing two fields in one dialog could disagree about.
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let content = content_bounds(&bounds, &self.metrics);
        // Render uses the previous frame's width for shaping. On the first
        // frame that width is zero, so its scroll offset would hide the entire
        // prefilled value. Resolve scrolling from this frame's real viewport
        // before any text, selection, or caret is painted.
        self.layout.scroll_to_caret(
            self.snapshot.selection.head,
            content.size.width,
            content.size.height,
        );
        {
            let mut geometry = self.geometry.borrow_mut();
            geometry.bounds = Some(bounds);
            // Handed to the next render so wrapping and scrolling have a real
            // width to work against instead of this frame's guess.
            geometry.content_width = f32::from(content.size.width);
        }
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );

        if self.snapshot.dragging {
            self.register_drag_listeners(window);
        }

        // Chrome first and unclipped: the field's own fill and border must not be
        // cut by the text area's mask.
        self.child.paint(window, cx);

        let content = content_bounds(&bounds, &self.metrics);
        let theme = global::current();
        let origin = point(
            content.origin.x - self.layout.scroll_x,
            content.origin.y - self.layout.scroll_y,
        );
        #[cfg(test)]
        self.geometry
            .borrow_mut()
            .first_painted_text_origin
            .get_or_insert(origin);

        window.with_content_mask(Some(ContentMask { bounds: content }), |window| {
            self.paint_selection(origin, &theme, window);
            paint_shaped(
                &self.layout.shaped,
                |line| self.layout.line_top(line),
                &self.metrics,
                origin,
                window,
                cx,
            );
            if let Some(placeholder) = &self.placeholder {
                paint_shaped(placeholder, |_| px(0.0), &self.metrics, origin, window, cx);
            }
            self.paint_caret(origin, &theme, window);
        });
    }
}

impl TextFieldElement {
    /// One rectangle per visual row the selection touches.
    fn paint_selection(&self, origin: Point<Pixels>, theme: &Theme, window: &mut Window) {
        let selection = self.snapshot.selection.range();
        for row in &self.layout.rows {
            let start = row.start.max(selection.start);
            let end = row.end.min(selection.end);
            if start >= end {
                continue;
            }
            let shape = &self.layout.lines[row.line].shape;
            let left = origin.x + row.x_for(start, shape);
            let right = origin.x + row.x_for(end, shape);
            paint_rect(
                window,
                left,
                origin.y + row.top,
                (right - left).max(px(0.0)),
                row.height,
                Colors::accent_muted(theme).into(),
            );
        }
    }

    /// A full-height rule at the selection head, on a blinking phase.
    ///
    /// Only while focused and enabled: a caret in a field that cannot be typed
    /// into is a lie about where the next character will go.
    fn paint_caret(&self, origin: Point<Pixels>, theme: &Theme, window: &mut Window) {
        if !(self.snapshot.focused && self.snapshot.enabled && self.snapshot.caret_on) {
            return;
        }
        let head = self.snapshot.selection.head;
        let Some(row) = self.layout.row_for_byte(head) else {
            return;
        };
        let shape = &self.layout.lines[row.line].shape;
        paint_rect(
            window,
            origin.x + row.x_for(head, shape),
            origin.y + row.top,
            px(CARET_WIDTH),
            row.height,
            Colors::accent(theme).into(),
        );
    }

    /// Track the pointer for the length of a drag, including outside the field.
    ///
    /// Registered on the window rather than on the element because a selection
    /// that runs past the last glyph has to keep growing after the pointer has
    /// left the field, and an element listener stops at its own hitbox.
    fn register_drag_listeners(&mut self, window: &mut Window) {
        let field = self.field.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            field.update(cx, |field, cx| {
                if !field.dragging || !field.enabled {
                    return;
                }
                let index = field.index_at(event.position, window);
                field.selection = field.selection.with_head(index, true);
                field.caret_on = true;
                cx.notify();
            });
        });

        let field = self.field.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, _phase, _window, cx| {
            field.update(cx, |field, cx| {
                if !field.dragging {
                    return;
                }
                field.dragging = false;
                cx.notify();
            });
        });
    }
}

impl IntoElement for TextFieldElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The text area inside the field's border and padding.
fn content_bounds(bounds: &Bounds<Pixels>, metrics: &FieldMetrics) -> Bounds<Pixels> {
    Bounds {
        origin: point(
            bounds.origin.x + metrics.inset_x,
            bounds.origin.y + metrics.inset_y,
        ),
        size: size(
            (bounds.size.width - metrics.inset_x * 2.0).max(px(0.0)),
            (bounds.size.height - metrics.inset_y * 2.0).max(px(0.0)),
        ),
    }
}

fn paint_rect(
    window: &mut Window,
    x: Pixels,
    y: Pixels,
    width: Pixels,
    height: Pixels,
    color: Background,
) {
    if width <= px(0.0) || height <= px(0.0) {
        return;
    }
    window.paint_quad(fill(
        Bounds {
            origin: point(x, y),
            size: size(width, height),
        },
        color,
    ));
}

// ── Layout ─────────────────────────────────────────────────────────────────

/// Shaped text, in whichever form the field's mode needs.
enum ShapedText {
    // Boxed because a `ShapedLine` carries its whole glyph run by value, which
    // would otherwise make every `ShapedText` thousands of bytes wide.
    Single(Box<ShapedLine>),
    Wrapped(Vec<WrappedLine>),
}

/// A logical line: a run of the value with no newline in it.
struct LogicalLine {
    /// Byte offset of the line's first character in the value.
    start: usize,
    shape: LineShape,
}

/// A logical line's glyphs, wrapped or not.
///
/// The wrapped case keeps the whole `WrappedLine` rather than reaching inside it
/// for a `WrappedLineLayout`: that field is crate-private, so the line itself is
/// the only handle that survives outside GPUI.
enum LineShape {
    Single(Arc<LineLayout>),
    Wrapped(Arc<WrappedLine>),
}

impl LineShape {
    /// The line without its wraps: every x measurement is against this one.
    fn unwrapped(&self) -> &Arc<LineLayout> {
        match self {
            LineShape::Single(layout) => layout,
            LineShape::Wrapped(line) => &line.unwrapped_layout,
        }
    }

    fn wrapped(&self) -> Option<&Arc<WrappedLine>> {
        match self {
            LineShape::Single(_) => None,
            LineShape::Wrapped(line) => Some(line),
        }
    }

    fn len(&self) -> usize {
        self.unwrapped().len
    }

    /// The line-relative byte range of visual row `row`.
    ///
    /// The wrap boundaries are read out of the shaped runs for the same reason
    /// GPUI's own layout reads them: the glyph index a boundary was recorded
    /// against is by construction a character boundary, and there is no other
    /// public accessor.
    fn row_bounds(&self, row: usize) -> (usize, usize) {
        let Some(wrapped) = self.wrapped() else {
            return (0, self.len());
        };
        let start = if row == 0 {
            0
        } else {
            boundary_index(wrapped, row - 1)
        };
        let end = match wrapped.wrap_boundaries().get(row) {
            Some(_) => boundary_index(wrapped, row),
            None => self.len(),
        };
        (start, end)
    }

    fn row_count(&self) -> usize {
        match self.wrapped() {
            Some(wrapped) => wrapped.wrap_boundaries().len() + 1,
            None => 1,
        }
    }

    /// Horizontal position of a line-relative byte offset, in the unwrapped
    /// line's own coordinates.
    fn x_in_line(&self, byte: usize) -> Pixels {
        self.unwrapped().x_for_index(byte.min(self.len()))
    }

    /// Where `row` starts horizontally inside its line: zero for a line's first
    /// row, and the wrap column for every row after it.
    fn row_origin_x(&self, row: usize) -> Pixels {
        if row == 0 {
            px(0.0)
        } else {
            self.x_in_line(self.row_bounds(row).0)
        }
    }

    /// The closest character boundary to a point expressed relative to the
    /// visual row, with `row_height` the height of one row.
    fn index_in_row(&self, row: usize, x: Pixels, row_height: Pixels) -> usize {
        match self.wrapped() {
            Some(wrapped) => {
                let within_row = x + self.row_origin_x(row);
                let at = point(within_row, row_height * row as f32);
                match wrapped.closest_index_for_position(at, row_height) {
                    Ok(index) | Err(index) => index.min(self.len()),
                }
            }
            None => self
                .unwrapped()
                .closest_index_for_x(x + self.row_origin_x(row))
                .min(self.len()),
        }
    }
}

/// The character index a wrap boundary sits at, taken from the glyph the
/// boundary was recorded against.
fn boundary_index(wrapped: &WrappedLine, boundary: usize) -> usize {
    let Some(boundary) = wrapped.wrap_boundaries().get(boundary) else {
        return wrapped.len();
    };
    wrapped
        .runs()
        .get(boundary.run_ix)
        .and_then(|run| run.glyphs.get(boundary.glyph_ix))
        .map_or_else(|| wrapped.len(), |glyph| glyph.index)
}

/// One visual row: a logical line's slice of text after wrapping.
#[derive(Debug)]
struct VisualRow {
    line: usize,
    /// Byte range of the value that this row shows.
    start: usize,
    end: usize,
    /// Byte offset of the logical line's own first character.
    line_start: usize,
    /// Left edge of the row inside its line; zero for a line's first row.
    origin_x: Pixels,
    /// Top of the row inside the text area.
    top: Pixels,
    height: Pixels,
    /// Row index inside its logical line, which the wrapped layout needs in
    /// order to resolve a point.
    index_in_line: usize,
}

impl VisualRow {
    /// Horizontal position of `byte`, which must be at or after this row's start.
    fn x_for(&self, byte: usize, shape: &LineShape) -> Pixels {
        let within = byte.saturating_sub(self.line_start);
        shape.x_in_line(within) - self.origin_x
    }

    fn width(&self, shape: &LineShape) -> Pixels {
        (self.x_for(self.end, shape) - self.x_for(self.start, shape)).max(px(0.0))
    }
}

/// The field's shaped text, with the scroll that keeps the caret visible.
///
/// Deliberately position-free: it is built during render, when the field's place
/// on screen is not yet known, and shifted into window coordinates at paint time.
struct TextLayout {
    shaped: ShapedText,
    lines: Vec<LogicalLine>,
    rows: Vec<VisualRow>,
    scroll_x: Pixels,
    scroll_y: Pixels,
    line_height: Pixels,
}

impl TextLayout {
    fn scroll_to_caret(&mut self, caret: usize, visible_width: Pixels, visible_height: Pixels) {
        let text_width = self
            .rows
            .iter()
            .map(|row| row.width(&self.lines[row.line].shape))
            .fold(px(0.0), Pixels::max);
        let text_height = self.line_height * self.rows.len() as f32;
        let (scroll_x, scroll_y) = match self.row_for_byte(caret) {
            Some(row) => (
                Self::horizontal_scroll(
                    row.x_for(caret, &self.lines[row.line].shape),
                    text_width,
                    visible_width,
                ),
                Self::vertical_scroll(row.top, text_height, visible_height),
            ),
            None => (px(0.0), px(0.0)),
        };
        self.scroll_x = scroll_x;
        self.scroll_y = scroll_y;
    }

    /// The top of a logical line's first visual row.
    fn line_top(&self, line: usize) -> Pixels {
        self.rows
            .iter()
            .find(|row| row.line == line && row.index_in_line == 0)
            .map_or(px(0.0), |row| row.top)
    }

    /// The row a byte offset falls on. A byte at a wrap boundary belongs to the
    /// row that ends there, not the one that starts there, so the caret sits at
    /// the end of the text it was typed after.
    fn row_for_byte(&self, byte: usize) -> Option<&VisualRow> {
        self.rows
            .iter()
            .find(|row| byte >= row.start && byte <= row.end)
    }

    /// The byte offset a point selects, relative to the text area.
    fn index_at(&self, point: Point<Pixels>) -> usize {
        let first_top = self.rows.first().map_or(px(0.0), |row| row.top);
        let row = self
            .rows
            .iter()
            .find(|row| point.y >= row.top && point.y < row.top + row.height)
            // Outside the rows entirely: take the nearest edge. Refusing would
            // make a click in the padding do nothing at all.
            .or_else(|| {
                if point.y < first_top {
                    self.rows.first()
                } else {
                    self.rows.last()
                }
            });
        let Some(row) = row else {
            return 0;
        };
        let Some(line) = self.lines.get(row.line) else {
            return 0;
        };
        // The text system answers in offsets into the *logical* line, so the
        // row's own start has to come back off before the offset is used as one
        // into the value.
        let in_line = line
            .shape
            .index_in_row(row.index_in_line, point.x, self.line_height);
        row.start + in_line.saturating_sub(row.start - row.line_start)
    }

    /// The horizontal offset that keeps `caret_x` inside `visible_width` of a
    /// `text_width`-wide text area.
    ///
    /// Recomputed from scratch every frame rather than accumulated: a value that
    /// depended on the previous scroll would have to be reset whenever the text
    /// shrank, and one reset is one bug.
    fn horizontal_scroll(caret_x: Pixels, text_width: Pixels, visible_width: Pixels) -> Pixels {
        let max_scroll = (text_width - visible_width).max(px(0.0));
        if max_scroll <= px(0.0) {
            return px(0.0);
        }
        let margin = px(CARET_WIDTH).min(visible_width);
        let scroll = if caret_x > visible_width - margin {
            caret_x - visible_width + margin
        } else {
            px(0.0)
        };
        scroll.clamp(px(0.0), max_scroll)
    }

    /// The vertical equivalent of [`TextLayout::horizontal_scroll`].
    fn vertical_scroll(caret_top: Pixels, text_height: Pixels, visible_height: Pixels) -> Pixels {
        let max_scroll = (text_height - visible_height).max(px(0.0));
        if max_scroll <= px(0.0) {
            return px(0.0);
        }
        let scroll = if caret_top > visible_height {
            caret_top - visible_height
        } else {
            px(0.0)
        };
        scroll.clamp(px(0.0), max_scroll)
    }
}

/// Shape the value and work out where every visual row and the caret are.
fn build_layout(
    window: &Window,
    metrics: &FieldMetrics,
    value: &str,
    visible_width: Pixels,
    caret: usize,
    color: gpui::Rgba,
    marked: Option<Range<usize>>,
) -> TextLayout {
    let shaped = shape_value(window, metrics, value, visible_width, color, marked);

    let mut lines: Vec<LogicalLine> = Vec::new();
    match &shaped {
        ShapedText::Single(line) => lines.push(LogicalLine {
            start: 0,
            shape: LineShape::Single(Arc::clone(&***line)),
        }),
        ShapedText::Wrapped(wrapped) => {
            let mut start = 0;
            for line in wrapped {
                let len = line.len();
                lines.push(LogicalLine {
                    start,
                    shape: LineShape::Wrapped(Arc::new(line.clone())),
                });
                // `shape_text` splits on '\n' and reports each line without it.
                start += len + 1;
            }
        }
    }

    let mut rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let shape = &line.shape;
        for row in 0..shape.row_count() {
            let (from, to) = shape.row_bounds(row);
            rows.push(VisualRow {
                line: index,
                start: line.start + from,
                end: line.start + to,
                line_start: line.start,
                origin_x: shape.row_origin_x(row),
                top: metrics.line_height * rows.len() as f32,
                height: metrics.line_height,
                index_in_line: row,
            });
        }
    }
    // An empty value still needs one row: without it there is nowhere to put the
    // caret or to click into.
    if rows.is_empty() {
        rows.push(VisualRow {
            line: 0,
            start: 0,
            end: 0,
            line_start: 0,
            origin_x: px(0.0),
            top: px(0.0),
            height: metrics.line_height,
            index_in_line: 0,
        });
    }

    let visible_rows = rows
        .len()
        .clamp(1, metrics.max_visible_rows.ceil().max(1.0) as usize);
    let visible_height = metrics.line_height * visible_rows as f32;

    let mut layout = TextLayout {
        shaped,
        lines,
        scroll_x: px(0.0),
        scroll_y: px(0.0),
        rows,
        line_height: metrics.line_height,
    };
    layout.scroll_to_caret(snap_back(value, caret), visible_width, visible_height);
    layout
}

/// Shape the value, wrapping only when there is a width to wrap against.
fn shape_value(
    window: &Window,
    metrics: &FieldMetrics,
    value: &str,
    visible_width: Pixels,
    color: gpui::Rgba,
    marked: Option<Range<usize>>,
) -> ShapedText {
    let runs = value_runs(value, metrics, color, marked);
    if metrics.wrap && visible_width > px(0.0) {
        match window.text_system().shape_text(
            SharedString::from(value.to_string()),
            metrics.font_size,
            &runs,
            Some(visible_width),
            None,
        ) {
            Ok(lines) => return ShapedText::Wrapped(lines.into_vec()),
            // Shaping well-formed UTF-8 does not fail, but a field must never
            // take the window down on the off chance.
            Err(error) => tracing::warn!(%error, "text field could not wrap its value"),
        }
    }
    // `shape_line` debug-asserts on newlines, and a single-line value cannot
    // hold one; the replace keeps that true even if a caller reaches here with
    // something else. A space is as long as a newline, so the runs still fit.
    let flattened = value.replace('\n', " ");
    ShapedText::Single(Box::new(window.text_system().shape_line(
        SharedString::from(flattened),
        metrics.font_size,
        &runs,
        None,
    )))
}

fn shape_one_line(
    window: &Window,
    text: &str,
    metrics: &FieldMetrics,
    color: gpui::Rgba,
) -> ShapedLine {
    window.text_system().shape_line(
        SharedString::from(text.to_string()),
        metrics.font_size,
        &[text_run(text, metrics, color)],
        None,
    )
}

/// The value's runs, with the IME's in-progress composition underlined the way
/// the system draws marked text.
fn value_runs(
    text: &str,
    metrics: &FieldMetrics,
    color: gpui::Rgba,
    marked: Option<Range<usize>>,
) -> Vec<TextRun> {
    let base = text_run(text, metrics, color);
    let Some(marked) = marked.filter(|range| range.start < range.end && range.end <= text.len())
    else {
        return vec![base];
    };
    let piece = |len, underline| TextRun {
        len,
        underline,
        ..base.clone()
    };
    let underline = gpui::UnderlineStyle {
        thickness: px(MARKED_UNDERLINE),
        color: Some(color.into()),
        wavy: false,
    };
    let mut runs = Vec::with_capacity(3);
    if marked.start > 0 {
        runs.push(piece(marked.start, None));
    }
    runs.push(piece(marked.len(), Some(underline)));
    if marked.end < text.len() {
        runs.push(piece(text.len() - marked.end, None));
    }
    runs
}

fn text_run(text: &str, metrics: &FieldMetrics, color: gpui::Rgba) -> TextRun {
    TextRun {
        len: text.len(),
        font: metrics.font.clone(),
        color: color.into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Paint already-shaped text.
///
/// `line_top` gives the top of a logical line's first *visual* row. It cannot be
/// the line's own index times the line height: a wrapped paragraph's second
/// logical line starts several rows down, and `WrappedLine::paint` draws every
/// one of its rows from wherever it is handed.
fn paint_shaped(
    shaped: &ShapedText,
    line_top: impl Fn(usize) -> Pixels,
    metrics: &FieldMetrics,
    origin: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    match shaped {
        ShapedText::Single(line) => {
            if let Err(error) = line.paint(origin, metrics.line_height, window, cx) {
                tracing::warn!(%error, "text field could not paint its line");
            }
        }
        ShapedText::Wrapped(lines) => {
            for (index, line) in lines.iter().enumerate() {
                let at = point(origin.x, origin.y + line_top(index));
                if let Err(error) =
                    line.paint(at, metrics.line_height, TextAlign::Left, None, window, cx)
                {
                    tracing::warn!(%error, "text field could not paint a wrapped row");
                    break;
                }
            }
        }
    }
}

// ── Selection ──────────────────────────────────────────────────────────────

/// A caret and an anchor, in UTF-8 byte offsets.
///
/// Both are kept because shift-arrow has to move one without the other, and
/// because the platform asks which end is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: usize,
    head: usize,
}

impl Selection {
    const fn collapsed(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    const fn select_all(len: usize) -> Self {
        Self {
            anchor: 0,
            head: len,
        }
    }

    fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Move the head, keeping the anchor when extending and collapsing onto the
    /// new head when not.
    fn with_head(&self, head: usize, extend: bool) -> Self {
        if extend {
            Self {
                anchor: self.anchor,
                head,
            }
        } else {
            Self::collapsed(head)
        }
    }

    /// Keep the anchor where it is and move only the head, used by shift-click.
    fn with_anchor(&self, anchor: usize) -> Self {
        Self {
            anchor,
            head: self.head,
        }
    }
}

// ── Modifiers ──────────────────────────────────────────────────────────────

/// How far one press of an editing key reaches, from the modifier held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Character,
    /// Option: a word, for movement and deletion alike.
    Word,
    /// Cmd: the edge of the line, or of the value for Up and Down.
    Line,
}

impl Reach {
    /// Cmd wins over Option, as it does in AppKit: Cmd-Option-Left is a line
    /// move, not a word move.
    fn from_modifiers(modifiers: Modifiers) -> Self {
        if modifiers.secondary() {
            Self::Line
        } else if modifiers.alt {
            Self::Word
        } else {
            Self::Character
        }
    }
}

/// The Edit menu's commands, one per action, so the handlers share one body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

// ── Undo ───────────────────────────────────────────────────────────────────

/// What an edit was, for deciding whether the next one joins its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    /// Text from the keyboard or an input method, Return included.
    Typing,
    /// Backspace, at any reach.
    DeleteBackward,
    /// Forward delete, at any reach.
    DeleteForward,
    /// Cut or paste. Never joins anything: each is a step of its own.
    Clipboard,
}

/// The value and selection as they were before a group of edits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    value: String,
    selection: Selection,
}

/// A field's undo and redo stacks, with AppKit's coalescing.
///
/// Pure state, so the grouping rules are tested without a window. The caller
/// says when an edit is about to happen and where it left the caret; this
/// decides whether that edit opens a new step.
#[derive(Debug, Default)]
struct UndoHistory {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The group a following edit may join: its kind, and the selection its
    /// last edit left. An edit joins only when it is the same kind *and* starts
    /// exactly there, so typing over a selection or after a click is new.
    open: Option<(EditKind, Selection)>,
}

impl UndoHistory {
    /// Steps kept per field. Each holds a whole value, and the composer's is
    /// capped at 32,000 characters, so this bounds a field to a few megabytes.
    const LIMIT: usize = 100;

    /// Record the state before an edit, unless the edit continues the open
    /// group. Any edit at all makes the redo stack unreachable.
    fn will_edit(&mut self, kind: EditKind, before: Snapshot) {
        let joins = kind != EditKind::Clipboard && self.open == Some((kind, before.selection));
        if !joins {
            self.undo.push(before);
            if self.undo.len() > Self::LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    /// Note where an edit left the selection, which is where the next edit has
    /// to start in order to join it.
    fn did_edit(&mut self, kind: EditKind, after: Selection) {
        self.open = Some((kind, after));
    }

    /// End the open group, so the next edit starts a step of its own.
    fn break_group(&mut self) {
        self.open = None;
    }

    fn undo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let previous = self.undo.pop()?;
        self.redo.push(current);
        self.open = None;
        Some(previous)
    }

    fn redo(&mut self, current: Snapshot) -> Option<Snapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.open = None;
        Some(next)
    }

    fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

// ── Pure text helpers ──────────────────────────────────────────────────────

/// Clamp a range to the value and to character boundaries, so no edit can split
/// a multi-byte character even when the caller asks for something impossible.
fn sanitize_range(value: &str, range: Range<usize>) -> Range<usize> {
    let start = snap_back(value, range.start.min(range.end));
    let end = snap_forward(value, range.end.max(range.start));
    start..end
}

/// The largest character boundary at or below `index`.
fn snap_back(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The smallest character boundary at or above `index`.
fn snap_forward(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn prev_boundary(value: &str, index: usize) -> usize {
    if index == 0 {
        return 0;
    }
    snap_back(value, index - 1)
}

fn next_boundary(value: &str, index: usize) -> usize {
    if index >= value.len() {
        return value.len();
    }
    snap_forward(value, index + 1)
}

/// What a replacement may be, once `max_len` has had its say.
///
/// The cap counts characters, not bytes: a field capped at ten has room for ten
/// emoji, and truncating by bytes would cut one in half.
fn truncate_to_max_len(value: &str, range: &Range<usize>, insert: &str, max_len: usize) -> String {
    if max_len == 0 {
        return insert.to_string();
    }
    let current = value.chars().count() - value[range.clone()].chars().count();
    let room = max_len.saturating_sub(current);
    if insert.chars().count() <= room {
        return insert.to_string();
    }
    insert.chars().take(room).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Whitespace,
    Word,
    Punctuation,
}

fn class_of(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

/// The word around `at`, for double-click selection.
pub(crate) fn word_range(value: &str, at: usize) -> Range<usize> {
    let at = snap_back(value, at);
    let Some(first) = value[at..].chars().next() else {
        return at..at;
    };
    let class = class_of(first);
    let mut start = at;
    while let Some(previous) = value[..start].chars().next_back() {
        if class_of(previous) != class {
            break;
        }
        start -= previous.len_utf8();
    }
    let mut end = at + first.len_utf8();
    while let Some(next) = value[end..].chars().next() {
        if class_of(next) != class {
            break;
        }
        end += next.len_utf8();
    }
    start..end
}

/// Where Option-Left lands, and what Option-Backspace deletes back to: the
/// start of the previous word.
///
/// Whitespace is crossed on the way rather than being a stop of its own, as in
/// AppKit: from "foo bar |" the word is "bar", and from "foo |" it is "foo", so
/// one press always removes a word and never just the space before it.
fn prev_word_boundary(value: &str, at: usize) -> usize {
    let head = value[..snap_back(value, at)].trim_end();
    let Some(last) = head.chars().next_back() else {
        return 0;
    };
    let class = class_of(last);
    // One past the last character of a different class.
    head.char_indices()
        .rev()
        .find(|(_, c)| class_of(*c) != class)
        .map_or(0, |(index, c)| index + c.len_utf8())
}

/// Where Option-Right lands, and what Option-Delete deletes up to: the end of
/// the next word, crossing any whitespace before it.
fn next_word_boundary(value: &str, at: usize) -> usize {
    let at = snap_back(value, at);
    let tail = &value[at..];
    let trimmed = tail.trim_start();
    let start = at + (tail.len() - trimmed.len());
    let Some(first) = trimmed.chars().next() else {
        return value.len();
    };
    let class = class_of(first);
    trimmed
        .char_indices()
        .find(|(_, c)| class_of(*c) != class)
        .map_or(value.len(), |(index, _)| start + index)
}

/// Move the caret one character or one word, optionally extending.
fn move_horizontal(
    value: &str,
    selection: Selection,
    forward: bool,
    extend: bool,
    by_word: bool,
) -> Selection {
    let anchor = if extend {
        selection.anchor
    } else {
        selection.head
    };
    // An arrow on a selection starts from the edge it is heading towards, which
    // is what makes shrink-then-grow feel right.
    let head = if selection.is_empty() {
        selection.head
    } else if forward {
        selection.range().end
    } else {
        selection.range().start
    };
    let head = match (forward, by_word) {
        (true, true) => next_word_boundary(value, head),
        (true, false) => next_boundary(value, head),
        (false, true) => prev_word_boundary(value, head),
        (false, false) => prev_boundary(value, head),
    };
    if extend {
        Selection { anchor, head }
    } else {
        Selection::collapsed(head)
    }
}

/// Up or down, one line of the *value* — not one visual row, which would make a
/// wrapped paragraph jump by a fraction of a line at a time.
fn move_vertical(value: &str, selection: Selection, forward: bool, extend: bool) -> Selection {
    let anchor = if extend {
        selection.anchor
    } else {
        selection.head
    };
    let head = snap_back(value, selection.head);
    let (line_start, column) = line_and_column(value, head);
    let head = if forward {
        value[head..].find('\n').map_or(head, |offset| {
            next_line_start(value, head + offset + 1, column)
        })
    } else if line_start == 0 {
        head
    } else {
        previous_line_start(value, line_start, column)
    };
    Selection { anchor, head }
}

/// The byte offset a vertical move should land on.
///
/// A column past the end of a shorter line lands at that line's end. The column
/// is recomputed from where the caret actually is rather than remembered, so
/// holding Down through a short line and back up returns to that line's end; a
/// field that has to hold on to a desired column needs one more piece of state
/// than this one keeps.
///
/// The column counts characters, not bytes. A byte column carried onto another
/// line lands inside any multibyte character there, and slicing at it panics.
fn line_and_column(value: &str, at: usize) -> (usize, usize) {
    let at = snap_back(value, at);
    let start = value[..at].rfind('\n').map_or(0, |offset| offset + 1);
    (start, value[start..at].chars().count())
}

fn line_end(value: &str, line_start: usize) -> usize {
    value[line_start..]
        .find('\n')
        .map_or(value.len(), |offset| line_start + offset)
}

/// The byte offset `column` characters into the line at `line_start`, or the
/// line's end when it is shorter.
fn column_on_line(value: &str, line_start: usize, column: usize) -> usize {
    let end = line_end(value, line_start);
    value[line_start..end]
        .char_indices()
        .nth(column)
        .map_or(end, |(offset, _)| line_start + offset)
}

fn next_line_start(value: &str, from: usize, column: usize) -> usize {
    if from > value.len() {
        return value.len();
    }
    column_on_line(value, from, column)
}

fn previous_line_start(value: &str, line_start: usize, column: usize) -> usize {
    let previous_end = line_start - 1;
    let previous_start = value[..previous_end]
        .rfind('\n')
        .map_or(0, |offset| offset + 1);
    column_on_line(value, previous_start, column)
}

/// Home or End, on the caret's line.
fn move_to_line_edge(value: &str, at: usize, to_end: bool) -> usize {
    let (start, _) = line_and_column(value, at);
    if to_end {
        line_end(value, start)
    } else {
        start
    }
}

/// A Cmd-arrow move to the edge `edge` finds from an offset.
///
/// Extending measures from the head and keeps the anchor. Otherwise a selection
/// is left from the side it was heading towards, as an ordinary arrow does, so
/// Cmd-Left on a selection goes to the start of the line its start is on.
fn move_to_edge(
    selection: Selection,
    extend: bool,
    forward: bool,
    edge: impl Fn(usize) -> usize,
) -> Selection {
    if extend {
        return Selection {
            anchor: selection.anchor,
            head: edge(selection.head),
        };
    }
    let from = if selection.is_empty() {
        selection.head
    } else if forward {
        selection.range().end
    } else {
        selection.range().start
    };
    Selection::collapsed(edge(from))
}

/// UTF-16 length, which is the unit every `InputHandler` range is in.
fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// The byte offset for a UTF-16 offset, rounding *down* to a character boundary.
///
/// Rounding down rather than up matters for the ranges the platform sends: an
/// end that rounded up would delete a character the caller never named. The cost
/// is that an index landing between two halves of a surrogate pair resolves to
/// the character before it, which is also a place a caret can legally sit.
fn byte_index_for_utf16(value: &str, index: usize) -> usize {
    let mut utf16 = 0;
    for (byte, c) in value.char_indices() {
        let next = utf16 + c.len_utf16();
        if next > index {
            return byte;
        }
        utf16 = next;
    }
    value.len()
}

/// The UTF-16 offset for a byte offset.
fn utf16_index_for_byte(value: &str, index: usize) -> usize {
    value
        .get(..snap_back(value, index))
        .map_or_else(|| utf16_len(value), utf16_len)
}

/// Clamp a UTF-16 range onto the value, snapping both ends to character
/// boundaries and ordering them, since the platform is allowed to send a range
/// the value cannot satisfy.
fn utf16_range(value: &str, range: Range<usize>) -> Range<usize> {
    let start = byte_index_for_utf16(value, range.start.min(range.end));
    let end = byte_index_for_utf16(value, range.end.max(range.start));
    sanitize_range(value, start..end)
}

// ── Platform input ─────────────────────────────────────────────────────────

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = utf16_range(&self.value, range);
        *adjusted_range = Some(
            utf16_index_for_byte(&self.value, range.start)
                ..utf16_index_for_byte(&self.value, range.end),
        );
        self.value.get(range).map(|value| {
            if self.secret {
                "•".repeat(utf16_len(value))
            } else {
                value.to_string()
            }
        })
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if !self.enabled {
            return None;
        }
        let range = self.selection.range();
        Some(UTF16Selection {
            range: utf16_index_for_byte(&self.value, range.start)
                ..utf16_index_for_byte(&self.value, range.end),
            reversed: self.selection.head < self.selection.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| {
            utf16_index_for_byte(&self.value, range.start)
                ..utf16_index_for_byte(&self.value, range.end)
        })
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_text(replacement_range, text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mark_text(range_utf16, new_text, new_selected_range);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = utf16_range(&self.value, range_utf16);
        let range = self.display_index(range.start)..self.display_index(range.end);
        let display_value = self.display_value();
        let metrics = self.metrics(&global::current(), true);
        let content = content_bounds(&element_bounds, &metrics);
        let layout = build_layout(
            window,
            &metrics,
            &display_value,
            content.size.width,
            self.display_index(self.selection.head),
            Colors::fg(&global::current()),
            None,
        );
        let origin = point(
            content.origin.x - layout.scroll_x,
            content.origin.y - layout.scroll_y,
        );

        let mut union: Option<Bounds<Pixels>> = None;
        for row in &layout.rows {
            let start = row.start.max(range.start);
            let end = row.end.min(range.end);
            if start > end {
                continue;
            }
            let shape = &layout.lines[row.line].shape;
            let bounds = Bounds {
                origin: point(origin.x + row.x_for(start, shape), origin.y + row.top),
                size: size(
                    (row.x_for(end, shape) - row.x_for(start, shape)).max(px(0.0)),
                    row.height,
                ),
            };
            union = Some(match union {
                Some(existing) => existing.union(&bounds),
                None => bounds,
            });
        }
        union.or_else(|| {
            // An empty range still has a place: the IME uses it to put its
            // candidate window next to the caret.
            let caret = snap_back(&display_value, range.start);
            let row = layout.row_for_byte(caret)?;
            Some(Bounds {
                origin: point(origin.x, origin.y + row.top),
                size: size(px(0.0), row.height),
            })
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        Some(utf16_index_for_byte(
            &self.value,
            self.index_at(point, _window),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_fields_mask_glyphs_with_reversible_unicode_caret_positions() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).secret(true));
            field.update(cx, |field, cx| field.set_value("aé🜁z", cx));
            let field = field.read(cx);
            assert_eq!(field.display_value(), "••••");
            for (source, display) in [(0, 0), (1, 3), (3, 6), (7, 9), (8, 12)] {
                assert_eq!(field.display_index(source), display);
                assert_eq!(field.source_index(display), source);
            }
        });
    }

    #[test]
    fn secret_fields_do_not_copy_or_cut_secrets_to_the_clipboard() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).secret(true));
            cx.write_to_clipboard(ClipboardItem::new_string("previous clipboard".into()));
            field.update(cx, |field, cx| {
                field.set_value("test-secret", cx);
                field.selection = Selection::select_all(field.value.len());
                assert!(field.handle_clipboard_key("c", true, cx));
                assert!(field.handle_clipboard_key("x", true, cx));
                assert_eq!(field.value(), "test-secret");
            });
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("previous clipboard")
            );
        });
    }

    #[test]
    fn cutting_ordinary_text_removes_the_selection_after_copying() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                field.set_value("keep remove keep", cx);
                field.selection = Selection {
                    anchor: 5,
                    head: 12,
                };
                assert!(field.handle_clipboard_key("x", true, cx));
                assert_eq!(field.value(), "keep keep");
            });
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().as_deref(),
                Some("remove ")
            );
        });
    }

    #[test]
    fn platform_selection_and_text_ranges_remain_utf16_for_non_ascii_input() {
        let mut test = gpui::TestAppContext::single();
        test.add_window(|window, cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                field.set_value("aé🜁z", cx);
                field.selection = Selection { anchor: 3, head: 7 };
                assert_eq!(
                    field.selected_text_range(false, window, cx).unwrap().range,
                    2..4
                );
                let mut adjusted = None;
                assert_eq!(
                    field
                        .text_for_range(2..4, &mut adjusted, window, cx)
                        .as_deref(),
                    Some("🜁")
                );
                assert_eq!(adjusted, Some(2..4));
                field.secret = true;
                assert_eq!(
                    field
                        .text_for_range(2..4, &mut adjusted, window, cx)
                        .as_deref(),
                    Some("••")
                );
            });
            Probe { field }
        });
        test.refresh().unwrap();
    }

    #[test]
    fn ime_composition_replaces_its_marked_text_instead_of_committing_it() {
        let mut test = gpui::TestAppContext::single();
        test.add_window(|window, cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(4.0));
            field.update(cx, |field, cx| {
                field.set_value("🜁", cx);
                field.selection = Selection::collapsed(field.value.len());

                // Japanese "ka": k, then か, then confirmed with Return.
                field.replace_and_mark_text_in_range(None, "k", Some(1..1), window, cx);
                assert_eq!(field.value, "🜁k");
                // The emoji is two UTF-16 units, so the composition starts at 2.
                assert_eq!(field.marked_text_range(window, cx), Some(2..3));
                field.replace_and_mark_text_in_range(None, "か", Some(1..1), window, cx);
                assert_eq!(field.value, "🜁か", "the preedit replaces itself");
                assert_eq!(field.selection, Selection::collapsed(field.value.len()));
                field.replace_text_in_range(None, "か", window, cx);
                assert_eq!(field.value, "🜁か", "confirming does not insert twice");
                assert_eq!(field.marked_text_range(window, cx), None);

                // A dead key: Option-E shows ´, then E replaces it with é.
                field.replace_and_mark_text_in_range(None, "´", Some(1..1), window, cx);
                field.replace_text_in_range(None, "é", window, cx);
                assert_eq!(field.value, "🜁かé");

                // Empty preedit text cancels the composition and removes it.
                field.replace_and_mark_text_in_range(None, "ab", Some(0..1), window, cx);
                assert_eq!(field.selected_text(), "a", "selection inside the preedit");
                field.replace_and_mark_text_in_range(None, "", None, window, cx);
                assert_eq!(field.value, "🜁かé");
                assert_eq!(field.marked_text_range(window, cx), None);

                // Any other edit ends the composition.
                field.replace_and_mark_text_in_range(None, "x", None, window, cx);
                field.set_value("reset", cx);
                assert_eq!(field.marked_text_range(window, cx), None);
            });
            Probe { field }
        });
        test.refresh().unwrap();
    }

    #[test]
    fn an_empty_field_fills_its_parent_flex_width() {
        struct WidthProbe(Entity<TextField>);
        impl Render for WidthProbe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().flex().w(px(420.0)).child(self.0.clone())
            }
        }
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).placeholder("Project path")));
        let for_window = field.clone();
        test.add_window(move |_, _| WidthProbe(for_window));
        test.refresh().unwrap();
        test.read(|cx| {
            let geometry = field.read(cx).geometry.borrow();
            assert_eq!(geometry.bounds.unwrap().size.width, px(420.0));
            assert!(
                geometry.content_width > 380.0,
                "an empty field collapsed to its padding"
            );
        });
    }

    #[test]
    fn a_prefilled_focused_field_paints_its_title_inside_the_first_frame() {
        struct SeededProbe(Entity<TextField>);
        impl Render for SeededProbe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().flex().w(px(420.0)).child(self.0.clone())
            }
        }
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| {
            cx.new(|cx| {
                let mut field = TextField::new(cx).placeholder("Thread name");
                field.set_value("Existing thread title", cx);
                field
            })
        });
        let for_window = field.clone();
        test.add_window(move |window, cx| {
            for_window.read(cx).focus_handle().focus(window);
            SeededProbe(for_window)
        });
        test.refresh().unwrap();
        test.read(|cx| {
            let field = field.read(cx);
            assert_eq!(field.value(), "Existing thread title");
            let geometry = field.geometry.borrow();
            let metrics = field.metrics(&global::current(), true);
            let content = content_bounds(&geometry.bounds.unwrap(), &metrics);
            assert_eq!(content.size.width + metrics.inset_x * 2.0, px(420.0));
            assert_eq!(
                geometry.first_painted_text_origin.unwrap(),
                content.origin,
                "a title fitting the field must not scroll outside its paint mask"
            );
        });
    }

    // ── The rules that hold whatever the source of the text ────────────────

    #[test]
    fn a_cap_counts_characters_so_emoji_fit() {
        // Twelve bytes, three characters: a byte cap would cut the second emoji
        // in half and leave invalid UTF-8 in the value.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).max_len(3));
            let three = "🜁🜂🜃";
            field.update(cx, |field, _| field.edit(0..0, three));
            assert_eq!(field.read(cx).value(), three);
            // Byte 3 is inside the first emoji, and the caret is clamped outward
            // to whole characters — which drops the first emoji and lets the new
            // one in, rather than cutting one in half.
            field.update(cx, |field, _| field.edit(3..3, "🜄"));
            assert_eq!(field.read(cx).value(), "🜄🜂🜃");
            // And at a real character boundary with the cap already reached,
            // nothing is inserted at all.
            let full = field.read(cx).value().len();
            field.update(cx, |field, _| field.edit(full..full, "🜄"));
            assert_eq!(field.read(cx).value(), "🜄🜂🜃");
        });
    }

    #[test]
    fn a_cap_counts_plain_characters_too() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).max_len(4));
            field.update(cx, |field, _| field.edit(0..0, "abcdef"));
            assert_eq!(field.read(cx).value(), "abcd");
        });
    }

    #[test]
    fn a_cap_applies_to_a_replacement_as_well_as_an_insert() {
        // Otherwise restoring a draft is the one way past the cap.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).max_len(5));
            field.update(cx, |field, _| field.edit(0..0, "abcde"));
            field.update(cx, |field, _| field.edit(0..5, "xyzxyzxyz"));
            assert_eq!(field.read(cx).value(), "xyzxy");
        });
    }

    #[test]
    fn a_cap_never_splits_a_character_when_it_truncates() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).max_len(2));
            field.update(cx, |field, _| field.edit(0..0, "a🜁b"));
            assert_eq!(field.read(cx).value(), "a🜁");
        });
    }

    #[test]
    fn a_single_line_field_never_contains_a_newline() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, _| field.edit(0..0, "a\nb"));
            field.update(cx, |field, _| {
                let end = field.value().len();
                field.edit(end..end, "\n");
            });
            field.update(cx, |field, _| {
                let end = field.value().len();
                field.edit(end..end, "x\r\ny");
            });
            assert_eq!(field.read(cx).value(), "abxy");
        });
    }

    #[test]
    fn setting_a_value_honours_the_same_rules_as_typing() {
        // Otherwise restoring a draft is the one path that can put a newline in
        // a single-line field or run past its cap.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).max_len(3));
            field.update(cx, |field, cx| field.set_value("abcdef", cx));
            assert_eq!(field.read(cx).value(), "abc");
            field.update(cx, |field, cx| field.set_value("a\nb", cx));
            assert_eq!(field.read(cx).value(), "ab");
        });
    }

    #[test]
    fn setting_a_value_lands_the_caret_at_the_end() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| field.set_value("hello", cx));
            assert_eq!(field.read(cx).selection, Selection::collapsed(5));
        });
    }

    #[test]
    fn a_multiline_field_keeps_its_newlines() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(4.0));
            field.update(cx, |field, _| field.edit(0..0, "a\nb"));
            assert_eq!(field.read(cx).value(), "a\nb");
        });
    }

    #[test]
    fn multiline_never_claims_zero_visible_rows() {
        // Zero rows would leave the field with nowhere to draw a caret.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(0.0));
            assert_eq!(field.read(cx).multiline, Some(1.0));
        });
    }

    #[test]
    fn a_disabled_field_refuses_input() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| TextField::new(cx).enabled(false));
            field.update(cx, |field, cx| field.set_value("typed", cx));
            assert_eq!(field.read(cx).value(), "");
            field.update(cx, |field, _| field.edit(0..0, "x"));
            assert_eq!(field.read(cx).value(), "");
        });
    }

    // ── Editing ────────────────────────────────────────────────────────────

    #[test]
    fn an_edit_leaves_the_caret_after_the_inserted_text() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, _| field.edit(0..0, "héllo"));
            assert_eq!(
                field.read(cx).selection,
                Selection::collapsed("héllo".len())
            );
            assert!(field.read(cx).selection.is_empty());
        });
    }

    #[test]
    fn a_degenerate_range_does_not_panic() {
        // The platform is allowed to send a range the value cannot satisfy.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, _| field.edit(0..0, "hello"));
            // Out of bounds: clamped to the end, so the insert lands there.
            field.update(cx, |field, _| field.edit(99..200, "!"));
            assert_eq!(field.read(cx).value(), "hello!");
            // Reversed: ordered before use, so "el" is replaced by "?".
            let reversed = std::ops::Range { start: 3, end: 1 };
            field.update(cx, |field, _| field.edit(reversed, "?"));
            assert_eq!(field.read(cx).value(), "h?lo!");
        });
    }

    #[test]
    fn an_edit_never_splits_a_character() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, _| field.edit(0..0, "héllo"));
            // Byte 2 is inside the two-byte 'é'. Clamping *outward* puts the whole
            // character inside the range, so the edit replaces it rather than
            // producing invalid UTF-8.
            field.update(cx, |field, _| field.edit(2..2, "X"));
            assert_eq!(field.read(cx).value(), "hXllo");
            // The same range with nothing to insert removes the whole character.
            field.update(cx, |field, _| field.edit(1..2, ""));
            assert_eq!(field.read(cx).value(), "hllo");
        });
    }

    #[test]
    fn deleting_past_either_end_is_a_no_op() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, _| field.edit(0..0, "abc"));
            field.update(cx, |field, _| field.edit(0..0, ""));
            assert_eq!(field.read(cx).value(), "abc");
            field.update(cx, |field, _| field.edit(3..3, ""));
            assert_eq!(field.read(cx).value(), "abc");
        });
    }

    // ── Word and line movement ─────────────────────────────────────────────

    #[test]
    fn a_word_delete_crosses_the_whitespace_before_the_word() {
        // Option-Backspace on "foo bar" removes "bar", leaving "foo "; from
        // just after the space it removes "foo " — a word, never a lone space.
        assert_eq!(prev_word_boundary("foo bar", 7), 4);
        assert_eq!(prev_word_boundary("foo bar", 4), 0);
        assert_eq!(prev_word_boundary("foo", 3), 0);
        assert_eq!(prev_word_boundary("  foo", 5), 2);
        // Punctuation ends a word, so "bar" goes and "foo." stays.
        assert_eq!(prev_word_boundary("foo.bar", 7), 4);
        assert_eq!(prev_word_boundary("foo bar ", 8), 4);
        assert_eq!(
            prev_word_boundary("one\ntwo", 4),
            0,
            "a newline is crossed too"
        );
        assert_eq!(prev_word_boundary("   ", 3), 0);
    }

    #[test]
    fn word_boundaries_work_on_multibyte_text() {
        // 'é' is two bytes, so the space after it is at byte 3.
        assert_eq!(prev_word_boundary("aé b", 5), 4);
        assert_eq!(prev_word_boundary("aé b", 4), 0);
        assert_eq!(next_word_boundary("aé b", 0), 3);
        assert_eq!(next_word_boundary("aé b", 3), 5);
    }

    #[test]
    fn a_forward_word_delete_skips_leading_whitespace_then_one_word() {
        assert_eq!(next_word_boundary("foo bar", 0), 3);
        assert_eq!(next_word_boundary("foo bar", 3), 7);
        assert_eq!(next_word_boundary("foo", 0), 3);
        assert_eq!(next_word_boundary("foo   ", 3), 6);
        assert_eq!(next_word_boundary("", 0), 0);
    }

    /// Press `key` with `modifiers` in a field holding `value` with the caret at
    /// `caret`, and return the value and selection that result.
    fn press(
        value: &str,
        caret: usize,
        key: &str,
        modifiers: Modifiers,
        multiline: bool,
    ) -> (String, Selection) {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(|cx| {
                let field = TextField::new(cx);
                if multiline {
                    field.multiline(4.0)
                } else {
                    field
                }
            });
            field.update(cx, |field, cx| {
                field.set_value(value, cx);
                field.selection = Selection::collapsed(caret);
                let reach = Reach::from_modifiers(modifiers);
                field.handle_edit_key(key, modifiers.shift, reach, cx);
                (field.value.clone(), field.selection)
            })
        })
    }

    #[test]
    fn cmd_arrows_go_to_the_edges_of_the_line_and_option_arrows_by_word() {
        let value = "one two\nthree four";
        let cmd = Modifiers::command();
        let option = Modifiers::alt();
        // Caret in the middle of "three".
        assert_eq!(press(value, 10, "left", cmd, true).1.head, 8);
        assert_eq!(press(value, 10, "right", cmd, true).1.head, value.len());
        assert_eq!(press(value, 2, "right", cmd, true).1.head, 7);
        assert_eq!(press(value, 10, "left", option, true).1.head, 8);
        assert_eq!(press(value, 10, "right", option, true).1.head, 13);
        assert_eq!(press(value, 13, "right", option, true).1.head, value.len());
        // A plain arrow still moves one character.
        assert_eq!(press(value, 10, "left", Modifiers::none(), true).1.head, 9);
        // Shift extends from where the caret was.
        let extended = press(value, 10, "left", Modifiers::command_shift(), true).1;
        assert_eq!(
            extended,
            Selection {
                anchor: 10,
                head: 8
            }
        );
        // Cmd-Up and Cmd-Down reach either end of the whole value.
        assert_eq!(press(value, 10, "up", cmd, true).1.head, 0);
        assert_eq!(press(value, 2, "down", cmd, true).1.head, value.len());
    }

    #[test]
    fn cmd_backspace_deletes_to_the_line_start_and_option_backspace_a_word() {
        let value = "one two\nthree four";
        let (after, selection) = press(value, 13, "backspace", Modifiers::command(), true);
        assert_eq!(after, "one two\n four");
        assert_eq!(selection, Selection::collapsed(8));
        let (after, _) = press(value, value.len(), "backspace", Modifiers::alt(), true);
        assert_eq!(after, "one two\nthree ");
        let (after, _) = press(value, 14, "backspace", Modifiers::alt(), true);
        assert_eq!(after, "one two\nfour", "the word and the space after it");
        // At the very start of a line it joins the line to the one above.
        let (after, _) = press(value, 8, "backspace", Modifiers::command(), true);
        assert_eq!(after, "one twothree four");
        // Forward: Option-Delete takes a word, Cmd-Delete the rest of the line.
        let (after, _) = press(value, 8, "delete", Modifiers::alt(), true);
        assert_eq!(after, "one two\n four");
        let (after, _) = press(value, 3, "delete", Modifiers::command(), true);
        assert_eq!(after, "one\nthree four");
    }

    #[test]
    fn cmd_return_submits_a_multiline_field_and_option_return_breaks_the_line() {
        assert_eq!(press("ab", 1, "enter", Modifiers::alt(), true).0, "a\nb");
        assert_eq!(press("ab", 1, "enter", Modifiers::command(), true).0, "ab");
    }

    // ── Undo ───────────────────────────────────────────────────────────────

    /// Type `text` one character at a time, the way the platform delivers it.
    fn type_text(field: &mut TextField, text: &str) {
        for c in text.chars() {
            field.commit_text(None, c.encode_utf8(&mut [0; 4]));
        }
    }

    #[test]
    fn a_run_of_typing_undoes_as_one_step_and_redoes_as_one() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                type_text(field, "hello world");
                assert!(field.undo(cx));
                assert_eq!(field.value(), "");
                assert!(!field.undo(cx), "nothing older than the run");
                assert!(field.redo(cx));
                assert_eq!(field.value(), "hello world");
                assert_eq!(field.selection, Selection::collapsed(11));
                assert!(!field.redo(cx));
            });
        });
    }

    #[test]
    fn moving_the_caret_or_switching_to_deleting_starts_a_new_step() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                type_text(field, "abc");
                field.handle_edit_key("left", false, Reach::Character, cx);
                type_text(field, "X");
                field.handle_edit_key("backspace", false, Reach::Character, cx);
                field.handle_edit_key("backspace", false, Reach::Character, cx);
                assert_eq!(field.value(), "ac");

                assert!(field.undo(cx), "both backspaces are one step");
                assert_eq!(field.value(), "abXc");
                assert!(field.undo(cx), "the typing after the move is its own");
                assert_eq!(field.value(), "abc");
                assert_eq!(field.selection, Selection::collapsed(2));
                assert!(field.undo(cx));
                assert_eq!(field.value(), "");
            });
        });
    }

    #[test]
    fn cut_paste_and_typing_over_a_selection_are_each_their_own_step() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            cx.write_to_clipboard(ClipboardItem::new_string("!".into()));
            field.update(cx, |field, cx| {
                type_text(field, "one");
                field.edit_command(EditCommand::Paste, cx);
                field.edit_command(EditCommand::Paste, cx);
                field.edit_command(EditCommand::SelectAll, cx);
                type_text(field, "two");
                assert_eq!(field.value(), "two");

                field.undo(cx);
                assert_eq!(field.value(), "one!!");
                assert_eq!(
                    field.selection,
                    Selection::select_all(5),
                    "selection restored"
                );
                field.undo(cx);
                assert_eq!(field.value(), "one!", "two pastes are two steps");
                field.undo(cx);
                assert_eq!(field.value(), "one");
            });
        });
    }

    #[test]
    fn a_new_edit_after_undo_discards_the_redo_and_a_no_op_records_nothing() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                // Backspace in an empty field changes nothing to undo.
                field.handle_edit_key("backspace", false, Reach::Character, cx);
                assert!(!field.can_undo());
                type_text(field, "draft");
                field.undo(cx);
                assert!(field.can_redo());
                type_text(field, "x");
                assert!(!field.can_redo());
                assert!(!field.redo(cx));
                assert_eq!(field.value(), "x");
            });
        });
    }

    #[test]
    fn assigning_a_value_is_not_undoable() {
        // Clearing the composer after a send must not leave the sent prompt one
        // Cmd-Z away.
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let field = cx.new(TextField::new);
            field.update(cx, |field, cx| {
                type_text(field, "sent prompt");
                field.set_value("", cx);
                assert!(!field.undo(cx));
                assert_eq!(field.value(), "");
            });
        });
    }

    #[test]
    fn the_history_is_bounded() {
        let mut history = UndoHistory::default();
        for index in 0..UndoHistory::LIMIT + 10 {
            history.will_edit(
                EditKind::Clipboard,
                Snapshot {
                    value: index.to_string(),
                    selection: Selection::collapsed(0),
                },
            );
        }
        assert_eq!(history.undo.len(), UndoHistory::LIMIT);
        assert_eq!(history.undo[0].value, "10", "the oldest steps go first");
    }

    #[test]
    fn an_undo_group_never_splits_an_ime_composition() {
        let mut test = gpui::TestAppContext::single();
        test.add_window(|window, cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(4.0));
            field.update(cx, |field, cx| {
                type_text(field, "ab");
                // A click ends the typing run, so the composition opens a step.
                field.moved_caret();
                field.replace_and_mark_text_in_range(None, "k", Some(1..1), window, cx);
                // The IME moves its selection inside the preedit as it goes,
                // which must not read as the caret leaving the group.
                field.replace_and_mark_text_in_range(None, "かん", Some(1..1), window, cx);
                assert!(!field.undo(cx), "no undo while the IME is composing");
                assert_eq!(field.value, "abかん");
                field.replace_and_mark_text_in_range(None, "漢", Some(1..1), window, cx);
                field.replace_text_in_range(None, "漢", window, cx);
                type_text(field, "字");
                assert_eq!(field.value, "ab漢字");

                assert!(field.undo(cx));
                assert_eq!(field.value, "ab", "the composition and what followed it");
                assert!(field.undo(cx));
                assert_eq!(field.value, "");
            });
            Probe { field }
        });
        test.refresh().unwrap();
    }

    #[test]
    fn a_paste_during_a_composition_is_a_step_of_its_own() {
        let mut test = gpui::TestAppContext::single();
        test.add_window(|window, cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(4.0));
            field.update(cx, |field, cx| {
                type_text(field, "ab");
                field.moved_caret();
                field.replace_and_mark_text_in_range(None, "かん", Some(2..2), window, cx);
                cx.write_to_clipboard(gpui::ClipboardItem::new_string("PASTED".into()));
                field.edit_command(EditCommand::Paste, cx);
                assert_eq!(field.value, "abかんPASTED");

                assert!(field.undo(cx));
                assert_eq!(
                    field.value, "abかん",
                    "Undo took the paste back together with the composition"
                );
            });
            Probe { field }
        });
        test.refresh().unwrap();
    }

    #[test]
    fn the_edit_menu_reaches_the_focused_field_through_its_actions() {
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        let mounted = field.clone();
        let window = test.add_window(move |_, _| Probe { field: mounted });
        window
            .update(&mut test, |_, window, cx| {
                field.read(cx).focus_handle().focus(window);
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|cx| field.update(cx, |field, _| type_text(field, "menu")));
        window
            .update(&mut test, |_, window, cx| {
                assert!(window.is_action_available(&Undo, cx), "Undo is enabled");
                window.dispatch_action(Box::new(Undo), cx);
            })
            .unwrap();
        test.run_until_parked();
        test.read(|cx| assert_eq!(field.read(cx).value(), ""));
        window
            .update(&mut test, |_, window, cx| {
                window.dispatch_action(Box::new(Redo), cx);
                window.dispatch_action(Box::new(SelectAll), cx);
                window.dispatch_action(Box::new(Cut), cx);
            })
            .unwrap();
        test.run_until_parked();
        test.read(|cx| {
            assert_eq!(field.read(cx).value(), "");
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("menu")
            );
        });
    }

    #[test]
    fn word_selection_covers_a_run_of_like_characters() {
        assert_eq!(word_range("hello world", 3), 0..5);
        assert_eq!(word_range("hello world", 7), 6..11);
        assert_eq!(word_range("a.b", 2), 2..3);
        assert_eq!(word_range("", 0), 0..0);
    }

    #[test]
    fn arrows_move_the_caret_and_shift_arrows_extend_from_the_anchor() {
        let moved = move_horizontal("abc", Selection::collapsed(1), true, false, false);
        assert_eq!(moved, Selection::collapsed(2));
        let extended = move_horizontal("abc", Selection::collapsed(1), true, true, false);
        assert_eq!(extended.anchor, 1);
        assert_eq!(extended.head, 2);
        assert_eq!(extended.range(), 1..2);
    }

    #[test]
    fn an_arrow_never_moves_past_either_end() {
        let at_end = move_horizontal("abc", Selection::collapsed(3), true, false, false);
        assert_eq!(at_end.head, 3);
        let at_start = move_horizontal("abc", Selection::collapsed(0), false, false, false);
        assert_eq!(at_start.head, 0);
    }

    #[test]
    fn an_arrow_on_a_selection_starts_from_the_edge_it_headed_towards() {
        let selected = Selection { anchor: 0, head: 3 };
        assert_eq!(
            move_horizontal("abcdef", selected, false, false, false).head,
            0
        );
        assert_eq!(
            move_horizontal("abcdef", selected, true, false, false).head,
            4
        );
    }

    #[test]
    fn home_and_end_work_on_the_carets_own_line() {
        let value = "one\ntwo\nthree";
        assert_eq!(move_to_line_edge(value, 5, false), 4);
        assert_eq!(move_to_line_edge(value, 5, true), 7);
        assert_eq!(move_to_line_edge(value, 0, false), 0);
        assert_eq!(move_to_line_edge(value, value.len(), true), value.len());
    }

    #[test]
    fn vertical_movement_keeps_the_column_and_clamps_to_a_short_line() {
        let value = "long line\nx\nlong line";
        // Column 2 of line 0 is column 1 of line 1, which is one character long,
        // so the caret lands at the end of that line.
        let down = move_vertical(value, Selection::collapsed(2), true, false);
        assert_eq!(down.head, 11);
        // Coming back up uses the column the caret actually has rather than the
        // one it had before the excursion — see `move_vertical`.
        let back = move_vertical(value, down, false, false);
        assert_eq!(back.head, 1);
        // Past the last line is a no-op rather than a panic.
        let last = move_vertical(value, Selection::collapsed(value.len() - 1), true, false);
        assert_eq!(last.head, value.len() - 1);
    }

    #[test]
    fn vertical_movement_stops_at_the_first_line() {
        let value = "one\ntwo";
        // Column 1 of "two" is column 1 of "one".
        let up = move_vertical(value, Selection::collapsed(5), false, false);
        assert_eq!(up.head, 1);
        let still = move_vertical(value, Selection::collapsed(1), false, false);
        assert_eq!(still.head, 1);
    }

    #[test]
    fn vertical_movement_counts_characters_across_multibyte_lines() {
        // Column 3 in bytes is inside the second `é`; slicing there panicked.
        let value = "éé\nabc";
        let up = move_vertical(value, Selection::collapsed(value.len()), false, false);
        assert_eq!(up.head, "éé".len());
        let down = move_vertical(value, up, true, false);
        assert_eq!(down.head, "éé\nab".len());

        // Column 1 in characters is one `é` in, not one byte in.
        let value = "a\néé";
        let down = move_vertical(value, Selection::collapsed(1), true, false);
        assert_eq!(down.head, "a\né".len());
        assert_eq!(
            move_vertical(value, down, false, false).head,
            1,
            "back to column 1 on the ASCII line"
        );

        // Every caret position moves to a character boundary in both directions.
        let value = "🜁é\nxyz\nñ🜁ö";
        for at in 0..=value.len() {
            if !value.is_char_boundary(at) {
                continue;
            }
            for forward in [false, true] {
                let moved = move_vertical(value, Selection::collapsed(at), forward, false);
                assert!(value.is_char_boundary(moved.head), "{at} {forward}");
            }
        }
    }

    // ── UTF-16, which is the unit the platform speaks ──────────────────────

    #[test]
    fn utf16_offsets_round_trip_through_multibyte_text() {
        let value = "a🜁b";
        assert_eq!(utf16_len(value), 4);
        for byte in 0..=value.len() {
            if value.is_char_boundary(byte) {
                assert_eq!(
                    byte_index_for_utf16(value, utf16_index_for_byte(value, byte)),
                    byte,
                    "byte {byte} did not round trip"
                );
            }
        }
    }

    #[test]
    fn a_utf16_offset_inside_a_character_rounds_down() {
        // The platform can name a range between surrogate halves; snapping down
        // is the only answer that keeps the value valid.
        let value = "a🜁b";
        assert_eq!(byte_index_for_utf16(value, 2), 1);
        assert_eq!(utf16_range(value, 2..2), 1..1);
    }

    #[test]
    fn an_impossible_utf16_range_is_ordered_and_clamped() {
        // A reversed range is the platform's to send, not the caller's to avoid.
        let reversed = std::ops::Range { start: 4, end: 2 };
        assert_eq!(utf16_range("abcdef", reversed), 2..4);
        assert_eq!(utf16_range("abcdef", 99..99), 6..6);
        assert_eq!(utf16_range("abcdef", 0..99), 0..6);
    }

    #[test]
    fn a_selection_reports_its_range_and_its_direction() {
        let backwards = Selection { anchor: 5, head: 1 };
        assert_eq!(backwards.range(), 1..5);
        assert!(!backwards.is_empty());
        assert_eq!(Selection::select_all(9).range(), 0..9);
    }

    // ── Scrolling, which is pure arithmetic on the shaped layout ───────────

    #[test]
    fn horizontal_scroll_is_zero_while_the_text_fits() {
        assert_eq!(
            TextLayout::horizontal_scroll(px(80.0), px(100.0), px(200.0)),
            px(0.0)
        );
    }

    #[test]
    fn horizontal_scroll_follows_a_caret_that_runs_off_the_right_edge() {
        assert_eq!(
            TextLayout::horizontal_scroll(px(250.0), px(400.0), px(200.0)),
            px(250.0 - 200.0 + CARET_WIDTH)
        );
        // And never past the end of the text.
        assert_eq!(
            TextLayout::horizontal_scroll(px(400.0), px(400.0), px(200.0)),
            px(200.0)
        );
    }

    #[test]
    fn vertical_scroll_appears_only_once_the_text_is_taller_than_the_field() {
        assert_eq!(
            TextLayout::vertical_scroll(px(0.0), px(40.0), px(80.0)),
            px(0.0)
        );
        assert_eq!(
            TextLayout::vertical_scroll(px(120.0), px(200.0), px(80.0)),
            px(40.0)
        );
    }

    // ── The tests that need a window ───────────────────────────────────────

    /// A one-line view whose only job is to give the field a `Render` stack.
    struct Probe {
        field: Entity<TextField>,
    }

    impl Render for Probe {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.field.clone().into_any_element()
        }
    }

    /// Mounts a field and paints it.
    ///
    /// A bare element cannot be painted here — GPUI's `current_view` unwraps a
    /// stack only a `Render` impl fills, and the field registers a frame
    /// callback while it draws — so this mounts a probe and paints that, the way
    /// the transcript tests do.
    fn paint(test: &mut gpui::TestAppContext, field: Entity<TextField>) {
        let window = test.add_window(move |_window, _cx| Probe { field });
        test.refresh().expect("the field painted");
        window
            .update(test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn an_empty_field_paints_its_placeholder() {
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).placeholder("Ask anything")));
        paint(&mut test, field);
    }

    #[test]
    fn a_filled_single_line_field_paints() {
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        test.update(|cx| field.update(cx, |field, _| field.edit(0..0, "hello world")));
        paint(&mut test, field);
    }

    #[test]
    fn a_field_paints_a_composition_in_either_layout() {
        for multiline in [false, true] {
            let mut test = gpui::TestAppContext::single();
            let field = test.update(|cx| {
                cx.new(|cx| {
                    let field = TextField::new(cx);
                    if multiline {
                        field.multiline(3.0)
                    } else {
                        field
                    }
                })
            });
            test.update(|cx| {
                field.update(cx, |field, _| {
                    field.edit(0..0, "aé\nz");
                    field.selection = Selection::collapsed(1);
                    field.mark_text(None, "かな", None);
                    assert_eq!(field.marked, Some(1..7));
                    let runs = value_runs(
                        &field.value,
                        &field.metrics(&global::current(), false),
                        Colors::fg(&global::current()),
                        field.marked.clone(),
                    );
                    let lengths: Vec<_> = runs.iter().map(|run| run.len).collect();
                    // A single-line field has already stripped the newline.
                    assert_eq!(lengths, [1, 6, field.value.len() - 7]);
                    assert!(runs[1].underline.is_some() && runs[0].underline.is_none());
                });
            });
            paint(&mut test, field);
        }
    }

    #[test]
    fn a_multiline_field_paints_text_long_enough_to_wrap() {
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).multiline(3.0)));
        test.update(|cx| {
            field.update(cx, |field, _| {
                field.edit(
                    0..0,
                    "first line\nsecond line, long enough that it has to wrap somewhere",
                );
            })
        });
        paint(&mut test, field);
    }

    /// Long enough to wrap several times at the widths these tests use.
    const WIDE: &str = "one two three four five six seven eight nine ten";

    /// Builds a layout against a known width and hands it to `check`.
    ///
    /// The only way to get a shaped layout out of GPUI is inside a render, and
    /// `add_window` is the one place a test is handed a `&mut Window` while
    /// building. Hit testing is the whole of mouse interaction, so it gets a
    /// real measurement rather than a stub.
    fn with_layout(value: &str, max_lines: f32, width: f32, check: impl FnOnce(&TextLayout)) {
        let value = value.to_string();
        let mut test = gpui::TestAppContext::single();
        let window = test.add_window(move |window, cx| {
            let field = cx.new(|cx| TextField::new(cx).multiline(max_lines));
            field.update(cx, |field, _| field.edit(0..0, &value));

            let theme = global::current();
            let metrics = field.read(cx).metrics(&theme, false);
            let layout = build_layout(
                window,
                &metrics,
                &value,
                px(width),
                0,
                Colors::fg(&theme),
                None,
            );
            check(&layout);
            Probe { field }
        });
        test.refresh().expect("the field painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn a_wrapped_row_hit_tests_inside_its_own_row() {
        // The text system answers in offsets into the logical line, so a click on
        // the second *visual* row has to have the first row's length subtracted
        // back off. Missing that lands the caret a whole row out.
        let mut rows_checked = 0;
        with_layout(WIDE, 3.0, 90.0, |layout| {
            assert!(layout.rows.len() > 2, "the value should have wrapped");
            for row in layout.rows.iter().skip(1) {
                let hit = layout.index_at(point(px(0.0), row.top + px(1.0)));
                assert!(
                    hit >= row.start && hit <= row.end,
                    "row {row:?} was hit-tested to byte {hit}"
                );
                rows_checked += 1;
            }
        });
        assert!(rows_checked > 1, "there was nothing to check");
    }

    #[test]
    fn a_click_past_the_end_of_a_wrapped_row_lands_on_the_wrap_boundary() {
        with_layout(WIDE, 3.0, 90.0, |layout| {
            let row = &layout.rows[1];
            let end = row.end;
            // Far to the right of the row's last glyph: the boundary, not the
            // value's end and not the row's start.
            let hit = layout.index_at(point(px(10_000.0), row.top + px(1.0)));
            assert_eq!(hit, end);
        });
    }

    #[test]
    fn a_click_in_a_single_line_field_clamps_to_the_text() {
        with_layout("hello", 1.0, 400.0, |layout| {
            assert_eq!(layout.rows.len(), 1);
            assert_eq!(layout.index_at(point(px(-50.0), px(1.0))), 0);
            assert_eq!(layout.index_at(point(px(10_000.0), px(1.0))), "hello".len());
        });
    }

    #[test]
    fn an_empty_field_still_has_a_row_for_the_caret_to_sit_in() {
        with_layout("", 1.0, 400.0, |layout| {
            assert_eq!(layout.rows.len(), 1);
            assert_eq!(layout.index_at(point(px(0.0), px(0.0))), 0);
        });
    }

    #[test]
    fn every_row_of_a_wrapped_field_measures_left_to_right() {
        // A row whose end is left of its start would draw a negative selection
        // and put the caret behind the text.
        with_layout(WIDE, 3.0, 90.0, |layout| {
            for row in &layout.rows {
                let shape = &layout.lines[row.line].shape;
                assert!(
                    row.x_for(row.start, shape) <= row.x_for(row.end, shape),
                    "row {row:?} measures right to left"
                );
            }
        });
    }

    #[test]
    fn a_focused_field_paints_its_caret() {
        // Focus is what starts the blink task and registers the platform input
        // handler, so neither is exercised by the other three.
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        test.update(|cx| field.update(cx, |field, _| field.edit(0..0, "focused")));
        let mounted = field.clone();
        let window = test.add_window(move |_window, _cx| Probe { field: mounted });
        window
            .update(&mut test, |_entity, window, cx| {
                field.read(cx).focus_handle().focus(window);
            })
            .expect("the window is live");
        test.refresh().expect("the focused field painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn a_disabled_field_paints() {
        let mut test = gpui::TestAppContext::single();
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).enabled(false)));
        paint(&mut test, field);
    }
}
