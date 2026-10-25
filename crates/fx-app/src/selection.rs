//! Selecting and copying read-only text in the transcript.
//!
//! GPUI 0.2.2 has no selectable text. The Swift original got it from
//! `NSTextView` one paragraph at a time, which is also why a selection there
//! could never cross from one message into the next. This module builds one
//! selection over the whole transcript instead, out of what GPUI already has:
//! every paragraph is a `StyledText`, and its `TextLayout` can map a point to a
//! byte offset and an offset back to a point.
//!
//! Three pieces, each small:
//!
//! - [`SelectableText`] wraps one paragraph. During prepaint it records the
//!   paragraph's text, layout, and bounds; during paint it draws the highlight
//!   for whatever part of the selection falls inside it, under the glyphs.
//! - [`scope`] wraps the transcript viewport. Its prepaint starts a fresh list,
//!   so the paragraphs register in document order once per frame, and it owns
//!   the pointer: a press on text anchors a selection and a drag moves its head.
//! - The selection itself is a pair of `(paragraph key, offset)` carets,
//!   resolved against the last frame's list whenever it is drawn or copied. A
//!   key is stable across frames, so a selection survives every repaint the
//!   stream causes, and a paragraph that disappears takes the selection with it
//!   rather than leaving it pointing at the wrong text.
//!
//! The transcript lays out only the rows in view, so the last frame's list is
//! a window onto the text rather than all of it. A scope therefore also holds
//! a [`Document`]: the same paragraphs, read from the conversation's data. A
//! caret whose row has scrolled away is placed by the document instead of
//! being dropped, and Select All and copying read whatever the frame does not
//! have from there. Painted layout is only ever used for what is on screen.
//!
//! Like the rest of the transcript's per-view state this lives in a
//! thread-local: GPUI renders on one thread, and nothing here is `Send`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use fx_design::theme::{Colors, global};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Element, ElementId,
    GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, KeyDownEvent,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    SharedString, StyledText, TextLayout, Window, fill, point, px, size,
};

/// Between two paragraphs of a copied selection.
const PARAGRAPH_BREAK: &str = "\n\n";
/// Between two lines that belong together, such as consecutive list items.
const LINE_BREAK: &str = "\n";

/// A position in the selectable text: a paragraph and a byte offset into it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Caret {
    key: SharedString,
    offset: usize,
}

/// One selectable paragraph, as the conversation's data describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paragraph {
    pub(crate) key: SharedString,
    /// Exactly the text the paragraph shapes when it is drawn.
    pub(crate) text: SharedString,
    /// Joined to the paragraph before it by a line break rather than a blank
    /// line when copied.
    pub(crate) tight: bool,
}

/// Everything a selection can cover, read from data rather than from layout.
///
/// Rows are the unit the transcript lays out: a row is either drawn whole or
/// not at all, so a caret is either among the last frame's paragraphs or in a
/// row the frame did not draw.
pub(crate) trait Document {
    /// How many rows there are.
    fn rows(&self) -> usize;
    /// The row a paragraph belongs to, while that row still exists.
    fn row_of(&self, key: &str) -> Option<usize>;
    /// One row's paragraphs, in document order.
    fn paragraphs(&self, row: usize) -> Vec<Paragraph>;
}

/// One selectable paragraph, as the last frame laid it out.
struct Run {
    key: SharedString,
    text: SharedString,
    layout: TextLayout,
    /// The part of the paragraph the reader can see. The pointer can only land
    /// on text it can see, so a prompt collapsed behind "Show more" or a row
    /// scrolled out of the viewport is never hit by a click aimed above it.
    visible: Bounds<Pixels>,
    /// Joined to the paragraph before it by a line break rather than a blank
    /// line when copied.
    tight: bool,
    /// The document row the paragraph belongs to. Every run is in row 0 when
    /// the scope has no document.
    row: usize,
}

/// One end of the selection, placed in document order.
///
/// Ordered by row first. Within a row, `paragraph` indexes the frame's runs
/// when the row was drawn and the row's own paragraphs when it was not; two
/// ends in one row are always both drawn or both not, so the two numberings
/// are never compared with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct End {
    row: usize,
    paragraph: usize,
    offset: usize,
    drawn: bool,
}

impl End {
    fn place(&self) -> (usize, usize) {
        (self.row, self.paragraph)
    }
}

/// The selection's resolved ends, kept for as long as nothing they were
/// resolved from has changed.
///
/// Every painted paragraph asks for its share of the selection, and an end
/// that has scrolled away is placed by reading its row from the document. Once
/// per frame is enough for that.
struct Resolved {
    anchor: Option<Caret>,
    head: Option<Caret>,
    ends: Option<(End, End)>,
}

#[derive(Default)]
struct State {
    /// The transcript the runs and the selection belong to.
    scope: Option<SharedString>,
    /// The transcript's paragraphs as data, for the rows the frame did not draw.
    document: Option<Rc<dyn Document>>,
    /// Every selectable paragraph the last frame drew, in document order.
    runs: Vec<Run>,
    index: HashMap<SharedString, usize>,
    /// Whether a scope is prepainting. Text outside one is not collected, so a
    /// paragraph drawn anywhere else cannot grow this list without bound.
    collecting: bool,
    anchor: Option<Caret>,
    head: Option<Caret>,
    dragging: bool,
    resolved: RefCell<Option<Resolved>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

impl State {
    /// Start collecting a new frame's paragraphs for `scope`.
    fn begin(&mut self, scope: &SharedString, document: Option<Rc<dyn Document>>) {
        if self.scope.as_ref() != Some(scope) {
            // A selection belongs to the transcript it was made in. Switching
            // threads and pressing Cmd-C must not copy text from the last one.
            self.clear();
            self.scope = Some(scope.clone());
        }
        self.document = document;
        self.runs.clear();
        self.index.clear();
        self.collecting = true;
        self.resolved.get_mut().take();
    }

    fn push(&mut self, mut run: Run) {
        // First registration wins, so a duplicated key cannot reorder the
        // document under a live selection.
        if self.index.contains_key(&run.key) {
            return;
        }
        // A paragraph the document cannot place stays with the one before it,
        // which keeps the runs in row order.
        run.row = self
            .document
            .as_ref()
            .and_then(|document| document.row_of(&run.key))
            .unwrap_or_else(|| self.runs.last().map_or(0, |last| last.row));
        self.index.insert(run.key.clone(), self.runs.len());
        self.runs.push(run);
        self.resolved.get_mut().take();
    }

    fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.dragging = false;
    }

    /// The frame's runs that belong to `row`, when the frame drew it.
    fn drawn(&self, row: usize) -> Option<Range<usize>> {
        let start = self.runs.partition_point(|run| run.row < row);
        let end = self.runs.partition_point(|run| run.row <= row);
        (start < end).then_some(start..end)
    }

    /// Place a caret in document order.
    ///
    /// A caret on screen resolves against the frame. One whose row was not
    /// drawn is placed by the document, which is what keeps a selection alive
    /// while its ends are scrolled away. A caret whose row *was* drawn without
    /// it has really gone — a prompt collapsed, a group closed — and takes the
    /// selection with it.
    fn resolve(&self, caret: &Caret) -> Option<End> {
        if let Some(&run) = self.index.get(&caret.key) {
            let drawn = &self.runs[run];
            return Some(End {
                row: drawn.row,
                paragraph: run,
                offset: snap(&drawn.text, caret.offset),
                drawn: true,
            });
        }
        let document = self.document.as_ref()?;
        let row = document.row_of(&caret.key)?;
        if self.drawn(row).is_some() {
            return None;
        }
        let paragraphs = document.paragraphs(row);
        let paragraph = paragraphs
            .iter()
            .position(|paragraph| paragraph.key == caret.key)?;
        Some(End {
            row,
            paragraph,
            offset: snap(&paragraphs[paragraph].text, caret.offset),
            drawn: false,
        })
    }

    /// The selection's two ends in document order, or `None` when nothing is
    /// selected or an end has gone out of existence.
    fn ends(&self) -> Option<(End, End)> {
        if let Some(resolved) = self.resolved.borrow().as_ref()
            && resolved.anchor == self.anchor
            && resolved.head == self.head
        {
            return resolved.ends;
        }
        let ends = (|| {
            let anchor = self.resolve(self.anchor.as_ref()?)?;
            let head = self.resolve(self.head.as_ref()?)?;
            let (start, end) = if anchor <= head {
                (anchor, head)
            } else {
                (head, anchor)
            };
            (start != end).then_some((start, end))
        })();
        *self.resolved.borrow_mut() = Some(Resolved {
            anchor: self.anchor.clone(),
            head: self.head.clone(),
            ends,
        });
        ends
    }

    /// The part of one run the selection covers.
    fn range_of(&self, run: usize) -> Option<Range<usize>> {
        let (start, end) = self.ends()?;
        covered(
            (self.runs[run].row, run),
            self.runs[run].text.len(),
            start,
            end,
        )
    }

    /// Whether the selection covers any text keyed under `prefix`.
    ///
    /// Asked while a row is being built, which is before this frame has drawn
    /// it: the frame's list starts empty and fills as rows paint. Reading only
    /// that list answered "no" for every row, so the menu never offered the
    /// selection and its Copy copied the whole message. A row the frame has
    /// not drawn yet is read from the document, as copying reads one.
    fn touches(&self, prefix: &str) -> bool {
        // Asked once per message on screen per pass, so with nothing selected
        // nothing is scanned.
        let Some((start, end)) = self.ends() else {
            return false;
        };
        let mut drawn = false;
        for index in 0..self.runs.len() {
            if self.runs[index].key.starts_with(prefix) {
                drawn = true;
                if self.range_of(index).is_some() {
                    return true;
                }
            }
        }
        if drawn {
            return false;
        }
        let Some(document) = self.document.as_ref() else {
            return false;
        };
        let Some(row) = document.row_of(prefix) else {
            return false;
        };
        if row < start.row || row > end.row || self.drawn(row).is_some() {
            return false;
        }
        if start.row < row && row < end.row {
            return true;
        }
        // A row an end is in: the part it covers may stop short of this
        // message's text, or begin after it.
        document
            .paragraphs(row)
            .iter()
            .enumerate()
            .any(|(index, paragraph)| {
                paragraph.key.starts_with(prefix)
                    && covered((row, index), paragraph.text.len(), start, end).is_some()
            })
    }

    /// The selected text, read from the frame for the rows it drew and from
    /// the document for the rest.
    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.ends()?;
        let mut text = String::new();
        let mut wrote_any = false;
        for row in start.row..=end.row {
            let paragraphs: Vec<(usize, SharedString, bool)> = match self.drawn(row) {
                Some(runs) => runs
                    .map(|run| (run, self.runs[run].text.clone(), self.runs[run].tight))
                    .collect(),
                None => self
                    .document
                    .as_ref()
                    .map(|document| {
                        document
                            .paragraphs(row)
                            .into_iter()
                            .enumerate()
                            .map(|(index, paragraph)| (index, paragraph.text, paragraph.tight))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            for (paragraph, body, tight) in paragraphs {
                let Some(range) = covered((row, paragraph), body.len(), start, end) else {
                    continue;
                };
                if wrote_any {
                    text.push_str(if tight { LINE_BREAK } else { PARAGRAPH_BREAK });
                }
                text.push_str(&body[range]);
                wrote_any = true;
            }
        }
        wrote_any.then_some(text)
    }

    /// The caret under the pointer, when it is over visible text.
    fn hit(&self, position: Point<Pixels>) -> Option<Caret> {
        let run = self
            .runs
            .iter()
            .find(|run| run.visible.contains(&position))?;
        Some(caret_in(run, position))
    }

    /// The caret nearest the pointer, for a drag that has left the text.
    ///
    /// Above a paragraph — including the gap between two — lands at its start;
    /// level with it lands where the layout says; below everything lands at the
    /// very end. That is what makes dragging down a margin select whole lines.
    fn nearest(&self, position: Point<Pixels>) -> Option<Caret> {
        if let Some(caret) = self.hit(position) {
            return Some(caret);
        }
        let mut last = None;
        for run in self.runs.iter().filter(|run| !run.visible.is_empty()) {
            if position.y < run.visible.top() {
                return Some(Caret {
                    key: run.key.clone(),
                    offset: 0,
                });
            }
            if position.y <= run.visible.bottom() {
                return Some(caret_in(run, position));
            }
            last = Some(run);
        }
        last.map(|run| Caret {
            key: run.key.clone(),
            offset: run.text.len(),
        })
    }

    /// A press in the transcript. Returns whether anything visible changed.
    fn press(&mut self, position: Point<Pixels>, clicks: usize, extend: bool) -> bool {
        let Some(caret) = self.hit(position) else {
            // Not on text: a button, a margin, an image. The selection goes,
            // as it would in any read-only text view, but no drag starts — a
            // press that began on a control is not a selection gesture.
            let had = self.anchor.is_some();
            self.clear();
            return had;
        };
        let text = self.resolve_text(&caret.key);
        match clicks {
            2 => {
                let word = crate::text_field::word_range(text.as_ref(), caret.offset);
                self.anchor = Some(Caret {
                    key: caret.key.clone(),
                    offset: word.start,
                });
                self.head = Some(Caret {
                    key: caret.key,
                    offset: word.end,
                });
                self.dragging = false;
            }
            count if count >= 3 => {
                self.anchor = Some(Caret {
                    key: caret.key.clone(),
                    offset: 0,
                });
                self.head = Some(Caret {
                    key: caret.key,
                    offset: text.len(),
                });
                self.dragging = false;
            }
            _ => {
                if !(extend && self.anchor.is_some()) {
                    self.anchor = Some(caret.clone());
                }
                self.head = Some(caret);
                self.dragging = true;
            }
        }
        true
    }

    /// The pointer moved. Returns whether the selection grew or shrank.
    fn drag(&mut self, position: Point<Pixels>, pressed: bool) -> bool {
        if !self.dragging {
            return false;
        }
        if !pressed {
            // The button went up somewhere this view never heard about.
            self.dragging = false;
            return false;
        }
        let Some(caret) = self.nearest(position) else {
            return false;
        };
        if self.head.as_ref() == Some(&caret) {
            return false;
        }
        self.head = Some(caret);
        true
    }

    fn release(&mut self) {
        self.dragging = false;
        if self.anchor.is_some() && self.anchor == self.head {
            // A click, not a selection: nothing should stay armed for Cmd-C.
            self.clear();
        }
    }

    /// Select from the first paragraph of the document to the end of its last.
    ///
    /// The document's ends rather than the frame's: the frame only drew the
    /// rows on screen, and Select All means the whole transcript.
    fn select_all(&mut self) -> bool {
        let from_document = self.document.as_ref().and_then(|document| {
            let rows = document.rows();
            let first = (0..rows).find_map(|row| document.paragraphs(row).into_iter().next())?;
            let last = (0..rows)
                .rev()
                .find_map(|row| document.paragraphs(row).pop())?;
            Some((first.key, (last.key, last.text.len())))
        });
        let from_frame = || {
            let (first, last) = (self.runs.first()?, self.runs.last()?);
            Some((first.key.clone(), (last.key.clone(), last.text.len())))
        };
        let Some((first, (last, length))) = from_document.or_else(from_frame) else {
            return false;
        };
        self.anchor = Some(Caret {
            key: first,
            offset: 0,
        });
        self.head = Some(Caret {
            key: last,
            offset: length,
        });
        self.dragging = false;
        true
    }

    fn resolve_text(&self, key: &SharedString) -> SharedString {
        self.index
            .get(key)
            .map(|run| self.runs[*run].text.clone())
            .unwrap_or_default()
    }
}

/// The caret a point inside a run's vertical span maps to.
fn caret_in(run: &Run, position: Point<Pixels>) -> Caret {
    // `Err` is the closest index when the point is past a line's end, which
    // is exactly where a drag beyond the last glyph should land.
    let offset = run
        .layout
        .index_for_position(position)
        .unwrap_or_else(|closest| closest);
    Caret {
        key: run.key.clone(),
        offset: snap(&run.text, offset),
    }
}

/// The part of the paragraph at `place`, `length` bytes long, that the
/// selection from `start` to `end` covers.
fn covered(place: (usize, usize), length: usize, start: End, end: End) -> Option<Range<usize>> {
    if place < start.place() || place > end.place() {
        return None;
    }
    let low = if place == start.place() {
        start.offset
    } else {
        0
    };
    let high = if place == end.place() {
        end.offset
    } else {
        length
    };
    (low < high).then_some(low..high)
}

/// Clamp an offset into the text and back onto a character boundary.
fn snap(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

// ── Public surface ─────────────────────────────────────────────────────────

/// The selected text, joined across paragraphs, if anything is selected.
pub(crate) fn selected_text() -> Option<String> {
    STATE.with(|state| state.borrow().selected_text())
}

/// Copy the selection. Returns whether there was one to copy.
pub(crate) fn copy_selection(cx: &mut App) -> bool {
    let Some(text) = selected_text() else {
        return false;
    };
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    true
}

/// Whether the selection covers any text whose key starts with `prefix`.
///
/// Every key in one message starts with that message's id, which is how a
/// message row decides whether its context menu should offer the selection.
pub(crate) fn touches(prefix: &str) -> bool {
    STATE.with(|state| state.borrow().touches(prefix))
}

/// The keyboard shortcuts a read-only selection answers. Returns whether the
/// key was claimed.
///
/// Cmd-C is claimed only when there is something to copy, so the shortcut
/// still reaches anything else that wants it.
pub(crate) fn handle_key(event: &KeyDownEvent, cx: &mut App) -> bool {
    let keystroke = &event.keystroke;
    if !keystroke.modifiers.secondary() || keystroke.modifiers.shift || keystroke.modifiers.alt {
        return false;
    }
    match keystroke.key.as_str() {
        "c" => copy_selection(cx),
        "a" => STATE.with(|state| state.borrow_mut().select_all()),
        _ => false,
    }
}

/// Drop the selection if it belongs to `scope`.
pub(crate) fn forget(scope: &str) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state
            .scope
            .as_ref()
            .is_some_and(|current| current.as_ref() == scope)
        {
            state.clear();
            state.runs.clear();
            state.index.clear();
            state.scope = None;
            state.document = None;
            state.resolved.get_mut().take();
        }
    });
}

// ── The paragraph element ──────────────────────────────────────────────────

/// One paragraph of selectable text.
pub(crate) struct SelectableText {
    key: SharedString,
    text: SharedString,
    layout: TextLayout,
    tight: bool,
    child: AnyElement,
}

impl SelectableText {
    /// Make `styled` selectable. `text` must be exactly what `styled` shapes,
    /// because offsets into one are used to slice the other.
    pub(crate) fn new(
        key: impl Into<SharedString>,
        text: SharedString,
        styled: StyledText,
    ) -> Self {
        Self::wrapping(key, text, styled, IntoElement::into_any_element)
    }

    /// Make text selectable that another element draws — a paragraph with
    /// links goes through `InteractiveText`, which owns the `StyledText`.
    pub(crate) fn wrapping(
        key: impl Into<SharedString>,
        text: SharedString,
        styled: StyledText,
        wrap: impl FnOnce(StyledText) -> AnyElement,
    ) -> Self {
        // The layout is a shared handle, filled in when the child is measured.
        // Holding a clone is what lets a later click be mapped back to text.
        let layout = styled.layout().clone();
        Self {
            key: key.into(),
            text,
            layout,
            tight: false,
            child: wrap(styled),
        }
    }

    /// Join this paragraph to the one before it with a line break when copied.
    pub(crate) fn tight(mut self, tight: bool) -> Self {
        self.tight = tight;
        self
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Hitbox> {
        self.child.prepaint(window, cx);
        if !STATE.with(|state| state.borrow().collecting) {
            return None;
        }
        // The content mask already includes every enclosing scroll viewport and
        // clip, so this is the part of the paragraph that is actually on screen.
        let visible = bounds.intersect(&window.content_mask().bounds);
        STATE.with(|state| {
            state.borrow_mut().push(Run {
                key: self.key.clone(),
                text: self.text.clone(),
                layout: self.layout.clone(),
                visible,
                tight: self.tight,
                row: 0,
            })
        });
        Some(window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Option<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(hitbox) = hitbox {
            // Set before the child paints, so a link's pointing hand wins
            // over the text cursor on the span it covers.
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
            let range = STATE.with(|state| {
                let state = state.borrow();
                let run = *state.index.get(&self.key)?;
                state.range_of(run)
            });
            if let Some(range) = range {
                paint_highlight(&self.layout, bounds, range, window);
            }
        }
        self.child.paint(window, cx);
    }
}

/// Draw the selection's rows behind the glyphs.
fn paint_highlight(
    layout: &TextLayout,
    bounds: Bounds<Pixels>,
    range: Range<usize>,
    window: &mut Window,
) {
    let (Some(start), Some(end)) = (
        layout.position_for_index(range.start),
        layout.position_for_index(range.end),
    ) else {
        return;
    };
    let color = Colors::text_selection(&global::current());
    for row in highlight_rows(
        start,
        end,
        layout.line_height(),
        bounds.left(),
        bounds.right(),
    ) {
        window.paint_quad(fill(row, color));
    }
}

/// The rectangles a selection from `start` to `end` covers.
///
/// Inner rows run the paragraph's full width, the way a native text view
/// draws them: the highlight says "these lines", not "these glyphs".
fn highlight_rows(
    start: Point<Pixels>,
    end: Point<Pixels>,
    line_height: Pixels,
    left: Pixels,
    right: Pixels,
) -> Vec<Bounds<Pixels>> {
    let row = |x0: Pixels, y: Pixels, x1: Pixels, height: Pixels| Bounds {
        origin: point(x0, y),
        size: size((x1 - x0).max(px(0.0)), height),
    };
    let mut rows = Vec::new();
    if end.y - start.y < line_height * 0.5 {
        rows.push(row(start.x, start.y, end.x, line_height));
    } else {
        rows.push(row(start.x, start.y, right, line_height));
        let middle = start.y + line_height;
        if end.y > middle {
            rows.push(row(left, middle, right, end.y - middle));
        }
        rows.push(row(left, end.y, end.x, line_height));
    }
    rows.retain(|row| row.size.width > px(0.0) && row.size.height > px(0.0));
    rows
}

// ── The scope element ──────────────────────────────────────────────────────

/// Make every [`SelectableText`] inside `content` part of one selection.
///
/// `scope` names the transcript, so a selection made in one thread is not
/// copied from another. `document` supplies the paragraphs `content` does not
/// draw this frame.
pub(crate) fn scope(
    scope: impl Into<SharedString>,
    document: Option<Rc<dyn Document>>,
    content: impl IntoElement,
) -> AnyElement {
    SelectionScope {
        scope: scope.into(),
        document,
        content: content.into_any_element(),
    }
    .into_any_element()
}

struct SelectionScope {
    scope: SharedString,
    document: Option<Rc<dyn Document>>,
    content: AnyElement,
}

impl IntoElement for SelectionScope {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectionScope {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        // Inserted before the content, so the paragraphs and controls inside
        // sit above it; an overlay that blocks the mouse sits above them all.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        STATE.with(|state| state.borrow_mut().begin(&self.scope, self.document.clone()));
        self.content.prepaint(window, cx);
        STATE.with(|state| state.borrow_mut().collecting = false);
        hitbox
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.paint(window, cx);

        let pressed = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _cx| {
            // Not stopped and not prevented: a press on a link or a button in
            // the transcript still has to reach it.
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !pressed.is_hovered(window)
            {
                return;
            }
            let changed = STATE.with(|state| {
                state
                    .borrow_mut()
                    .press(event.position, event.click_count, event.modifiers.shift)
            });
            if changed {
                window.refresh();
            }
        });
        // On the window rather than the element: a drag that leaves the
        // viewport has to keep extending the selection.
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let changed = STATE.with(|state| {
                state.borrow_mut().drag(
                    event.position,
                    event.pressed_button == Some(MouseButton::Left),
                )
            });
            if changed {
                window.refresh();
            }
        });
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, _cx| {
            if phase == DispatchPhase::Bubble {
                STATE.with(|state| state.borrow_mut().release());
            }
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A run whose layout was never measured. The selection logic never
    /// touches the layout, only the order, the text, and the visible bounds.
    fn run(key: &str, text: &str, top: f32, tight: bool) -> Run {
        Run {
            key: key.to_string().into(),
            text: text.to_string().into(),
            layout: TextLayout::default(),
            visible: Bounds {
                origin: point(px(0.0), px(top)),
                size: size(px(100.0), px(20.0)),
            },
            tight,
            row: 0,
        }
    }

    fn state(runs: Vec<Run>) -> State {
        framed(None, runs)
    }

    /// A frame that drew `runs`, over an optional document.
    fn framed(document: Option<Rc<dyn Document>>, runs: Vec<Run>) -> State {
        let mut state = State::default();
        state.begin(&"scope".into(), document);
        for run in runs {
            state.push(run);
        }
        state.collecting = false;
        state
    }

    /// Rows of paragraphs, keyed `"{row}:{index}"`, for a frame to draw part of.
    struct Rows(Vec<Vec<(&'static str, bool)>>);

    impl Document for Rows {
        fn rows(&self) -> usize {
            self.0.len()
        }

        fn row_of(&self, key: &str) -> Option<usize> {
            let row: usize = key.split(':').next()?.parse().ok()?;
            (row < self.0.len()).then_some(row)
        }

        fn paragraphs(&self, row: usize) -> Vec<Paragraph> {
            self.0[row]
                .iter()
                .enumerate()
                .map(|(index, (text, tight))| Paragraph {
                    key: format!("{row}:{index}").into(),
                    text: text.to_string().into(),
                    tight: *tight,
                })
                .collect()
        }
    }

    /// Five rows, the middle one with no text at all, of which a frame draws
    /// only row 2.
    fn scrolled_document() -> State {
        let document = Rows(vec![
            vec![("first row", false)],
            vec![("second row", false), ("one", true), ("two", true)],
            vec![("on screen", false)],
            vec![],
            vec![("last row", false)],
        ]);
        framed(
            Some(Rc::new(document)),
            vec![run("2:0", "on screen", 0.0, false)],
        )
    }

    #[test]
    fn a_row_being_built_knows_the_selection_touches_it() {
        // The row asks while it is built: the frame has begun and drawn
        // nothing yet, so only the document can answer.
        let document = Rows(vec![
            vec![("first row", false)],
            vec![("second row", false), ("one", true), ("two", true)],
            vec![("third row", false)],
            vec![("fourth row", false)],
        ]);
        let mut state = framed(Some(Rc::new(document)), Vec::new());
        state.anchor = caret("1:1", 1);
        state.head = caret("1:2", 2);
        assert!(state.touches("1:"));
        assert!(!state.touches("0:"));
        assert!(!state.touches("2:"));

        // Across rows: the middle is covered whole, and an end that stops at
        // the very end of a row's text covers none of it.
        state.anchor = caret("0:0", 9);
        state.head = caret("3:0", 3);
        assert!(!state.touches("0:"));
        assert!(state.touches("1:"));
        assert!(state.touches("2:"));
        assert!(state.touches("3:"));
    }

    #[test]
    fn a_selection_whose_ends_scrolled_away_copies_every_row_between_them() {
        let mut state = scrolled_document();
        state.anchor = caret("4:0", 4);
        state.head = caret("1:0", 7);
        assert_eq!(
            state.selected_text().as_deref(),
            Some("row\none\ntwo\n\non screen\n\nlast")
        );
        // The drawn paragraph between the two is highlighted whole.
        assert_eq!(state.range_of(0), Some(0..9));
    }

    #[test]
    fn an_end_on_screen_and_an_end_scrolled_away_meet_in_document_order() {
        let mut state = scrolled_document();
        state.anchor = caret("2:0", 3);
        state.head = caret("0:0", 6);
        assert_eq!(
            state.selected_text().as_deref(),
            Some("row\n\nsecond row\none\ntwo\n\non ")
        );
        assert_eq!(state.range_of(0), Some(0..3));
    }

    #[test]
    fn select_all_reaches_the_rows_the_frame_never_drew() {
        let mut state = scrolled_document();
        assert!(state.select_all());
        assert_eq!(
            state.selected_text().as_deref(),
            Some("first row\n\nsecond row\none\ntwo\n\non screen\n\nlast row")
        );
    }

    #[test]
    fn a_paragraph_missing_from_a_row_the_frame_drew_ends_the_selection() {
        // Its row is on screen, so the paragraph really has gone — a prompt
        // collapsed or a group closed — rather than scrolled away.
        let mut state = scrolled_document();
        state.anchor = caret("2:5", 0);
        state.head = caret("4:0", 3);
        assert!(state.selected_text().is_none());
        // A paragraph its document no longer has goes the same way.
        state.anchor = caret("1:9", 0);
        assert!(state.selected_text().is_none());
    }

    fn caret(key: &str, offset: usize) -> Option<Caret> {
        Some(Caret {
            key: key.to_string().into(),
            offset,
        })
    }

    #[test]
    fn a_selection_across_paragraphs_copies_each_part_in_document_order() {
        let mut state = state(vec![
            run("a", "first paragraph", 0.0, false),
            run("b", "second", 30.0, false),
            run("c", "third item", 60.0, true),
        ]);
        // Dragged upwards: the head is before the anchor.
        state.anchor = caret("c", 5);
        state.head = caret("a", 6);
        assert_eq!(
            state.selected_text().as_deref(),
            Some("paragraph\n\nsecond\nthird")
        );
        assert_eq!(state.range_of(0), Some(6..15));
        assert_eq!(state.range_of(1), Some(0..6));
        assert_eq!(state.range_of(2), Some(0..5));
    }

    #[test]
    fn a_collapsed_or_orphaned_selection_selects_nothing() {
        let mut state = state(vec![run("a", "text", 0.0, false)]);
        state.anchor = caret("a", 2);
        state.head = caret("a", 2);
        assert!(state.selected_text().is_none());
        // The paragraph the head was in is gone from this frame.
        state.head = caret("gone", 0);
        assert!(state.selected_text().is_none());
        assert!(state.range_of(0).is_none());
    }

    #[test]
    fn offsets_are_clamped_onto_character_boundaries() {
        let mut state = state(vec![run("a", "Açores", 0.0, false)]);
        state.anchor = caret("a", 2); // inside the two-byte "ç"
        state.head = caret("a", 99);
        assert_eq!(state.selected_text().as_deref(), Some("çores"));
    }

    #[test]
    fn a_press_off_text_clears_the_selection_without_starting_a_drag() {
        let mut state = state(vec![run("a", "text", 0.0, false)]);
        state.anchor = caret("a", 0);
        state.head = caret("a", 4);
        assert!(state.press(point(px(500.0), px(500.0)), 1, false));
        assert!(state.anchor.is_none() && !state.dragging);
        assert!(!state.drag(point(px(10.0), px(10.0)), true));
    }

    #[test]
    fn a_drag_past_the_text_lands_at_the_nearest_paragraph_edge() {
        let state = state(vec![
            run("a", "one", 0.0, false),
            run("b", "two", 40.0, false),
        ]);
        // In the gap between the two: the start of the next.
        assert_eq!(state.nearest(point(px(10.0), px(30.0))), caret("b", 0));
        // Below everything: the very end.
        assert_eq!(state.nearest(point(px(10.0), px(400.0))), caret("b", 3));
        // Above everything: the very start.
        assert_eq!(state.nearest(point(px(10.0), px(-40.0))), caret("a", 0));
    }

    #[test]
    fn a_click_without_a_drag_leaves_nothing_armed_for_copy() {
        let mut state = state(vec![run("a", "text", 0.0, false)]);
        state.anchor = caret("a", 1);
        state.head = caret("a", 1);
        state.dragging = true;
        state.release();
        assert!(state.anchor.is_none() && state.head.is_none());
    }

    #[test]
    fn select_all_spans_every_paragraph() {
        let mut state = state(vec![
            run("a", "one", 0.0, false),
            run("b", "two", 40.0, false),
        ]);
        assert!(state.select_all());
        assert_eq!(state.selected_text().as_deref(), Some("one\n\ntwo"));
    }

    #[test]
    fn switching_transcripts_drops_the_selection() {
        let mut state = state(vec![run("a", "one", 0.0, false)]);
        state.anchor = caret("a", 0);
        state.head = caret("a", 3);
        state.begin(&"another".into(), None);
        assert!(state.anchor.is_none());
    }

    #[test]
    fn highlight_rows_cover_partial_first_and_last_lines_and_full_inner_lines() {
        let line = px(20.0);
        let single = highlight_rows(
            point(px(10.0), px(0.0)),
            point(px(40.0), px(0.0)),
            line,
            px(0.0),
            px(100.0),
        );
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].size.width, px(30.0));

        let three = highlight_rows(
            point(px(10.0), px(0.0)),
            point(px(40.0), px(40.0)),
            line,
            px(0.0),
            px(100.0),
        );
        assert_eq!(three.len(), 3);
        assert_eq!(three[0].origin.x, px(10.0));
        assert_eq!(three[0].size.width, px(90.0));
        assert_eq!(three[1].size.width, px(100.0));
        assert_eq!(three[1].size.height, px(20.0));
        assert_eq!(three[2].size.width, px(40.0));
    }

    /// Where a paragraph was painted in the last frame, for driving a pointer
    /// at it from a view test.
    pub(crate) fn painted_bounds(key: &str) -> Option<Bounds<Pixels>> {
        STATE.with(|state| {
            let state = state.borrow();
            state
                .runs
                .iter()
                .find(|run| run.key.as_ref() == key)
                .map(|run| run.visible)
        })
    }

    /// Every paragraph the last frame drew, as `(key, text, tight)`.
    pub(crate) fn painted_paragraphs() -> Vec<(String, String, bool)> {
        STATE.with(|state| {
            state
                .borrow()
                .runs
                .iter()
                .map(|run| (run.key.to_string(), run.text.to_string(), run.tight))
                .collect()
        })
    }

    /// Every paragraph the scope's document holds, in the same shape.
    pub(crate) fn document_paragraphs() -> Vec<(String, String, bool)> {
        STATE.with(|state| {
            let state = state.borrow();
            let Some(document) = state.document.as_ref() else {
                return Vec::new();
            };
            (0..document.rows())
                .flat_map(|row| document.paragraphs(row))
                .map(|paragraph| {
                    (
                        paragraph.key.to_string(),
                        paragraph.text.to_string(),
                        paragraph.tight,
                    )
                })
                .collect()
        })
    }
}
