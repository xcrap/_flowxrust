//! The message composer: the prompt box, the controls under it, and the queue
//! above it.
//!
//! The text area is somebody else's — a [`TextField`] entity the caller owns and
//! hands in. Everything around it is here: the container, the primary action,
//! the model/effort/mode controls, and the strip that shows prompts waiting
//! behind the running turn.
//!
//! It is a function rather than a `Render` entity, for the same reason
//! [`crate::transcript`] is: the per-thread UI state a composer accumulates
//! (which thread it belongs to, whether it has been focused) is thin enough that
//! the caller's `AppState` can hold it, and a second owner of it would be a
//! second thing to be wrong.
//!
//! # The real signature
//!
//! ```ignore
//! pub fn view(
//!     field: &Entity<TextField>,
//!     chrome: &mut ComposerChrome,
//!     window: &mut Window,
//!     cx: &mut App,
//! ) -> gpui::AnyElement;
//! ```
//!
//! [`ComposerChrome`] is what the composer cannot derive from the field: whether
//! a turn is running, what is queued behind it, and whether it accepts input.
//! It also carries the three pickers' state and one callback.
//!
//! There is one callback, not one per control, because a method per callback is
//! a method per *decision*, and every decision here is the caller's. The caller
//! writes one match:
//!
//! ```ignore
//! let mut chrome = ComposerChrome {
//!     busy: conversation.is_busy(),
//!     queued: conversation.queued().map(|prompt| prompt.preview()).collect(),
//!     enabled: true,
//!     model,
//!     effort,
//!     mode,
//!     ..ComposerChrome::default()
//!     }
//!     .on_action(move |action, _window, cx| match action {
//!         ComposerAction::Submit => send(cx),
//!         ComposerAction::Stop => stop(cx),
//!         ComposerAction::Attach => attach(cx),
//!         ComposerAction::RemoveQueued(index) => dequeue(cx, index),
//!         ComposerAction::Select(ComposerPicker::Effort, value) => set_effort(cx, value),
//!         // The model and mode pickers are not wired yet.
//!         _ => {}
//!     });
//! composer::view(&field, &mut chrome, window, cx)
//! ```
//!
//! With no handler installed every control renders disabled. That is the same
//! rule the rest of the app follows: a control that cannot do anything must not
//! look live.
//!
//! `chrome` arrives as `&mut` because the composer is entitled to reconcile
//! chrome it owns. Nothing here does: every decision below is a read, and
//! mutating the caller's state from inside a render is the wrong place to start.
//! It is kept mutable so a later change — reclaiming focus, for one — does not
//! have to change the signature every caller already depends on.
//!
//! # What the Swift did that this does not
//!
//! Three omissions are deliberate rather than forgotten, and each one is a
//! decision that belongs above this file:
//!
//! - **Attachment chips and the drop target.** The button calls back and draws
//!   nothing: preparing an image needs the attachment pipeline, and a thumbnail
//!   needs a decode step this view has no business running.
//! - **Editing a queued prompt in place.** The strip removes a prompt and shows
//!   it; it does not edit it. An inline editor needs its own `TextField` per
//!   queued row, and its own focus owner, which is state this view does not
//!   hold.
//! - **`focusComposer()` on appear.** A function view cannot tell a first render
//!   from a repaint, and focusing on every pass would pull the caret out of the
//!   terminal and the browser. The caller focuses the field.
//!
//! The context ring around the primary control draws an empty track rather than
//! a real fill: `ComposerChrome` carries no usage reading, and
//! `UsageTotals::context_fraction` is the caller's to pass once the chrome grows
//! a field for it.

use std::rc::Rc;

use fx_agent::conversation::state::VISIBLE_QUEUED_PROMPTS;
use fx_design::components::{
    BadgeTone, ICON_BUTTON_SIZE, IconButtonProps, SegmentedOption, badge, context_ring_icon,
    icon_button, segmented_control,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{AnyElement, App, ClickEvent, Entity, SharedString, Window, div, px};

use crate::text_field::TextField;

/// The callback every control reports through.
///
/// A `Window` and an `App`, because that is all a GPUI click hands a callback
/// and the state has to be reachable without being captured.
type ActionHandler = Rc<dyn Fn(ComposerAction, &mut Window, &mut App)>;

/// Which of the three pickers changed.
///
/// Named rather than stringly typed so the caller cannot silently misspell one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposerPicker {
    /// The provider model.
    Model,
    /// The reasoning effort.
    Effort,
    /// The conversation mode (chat or plan).
    Mode,
}

/// Something a control asked for.
///
/// One enum rather than five callbacks: which controls exist is a layout
/// decision that can change, and a callback that survives the control being
/// removed is dead weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    /// Send the draft. Fired by the primary control when it is showing Send.
    Submit,
    /// Stop the running turn. Fired by the same control while it shows Stop.
    Stop,
    /// Open the attachment picker.
    Attach,
    /// A picker changed. The value is the caller's own vocabulary, not the
    /// composer's.
    Select(ComposerPicker, String),
    /// Remove the queued prompt at this position, counting from the front.
    ///
    /// The index is a position in the queue rather than in the visible strip,
    /// because `Conversation::queued_previews` takes from the front: the strip
    /// never shows a row whose index it could not name.
    RemoveQueued(usize),
}

/// One exclusive choice in a picker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choice {
    /// What the caller calls it — a model id, an effort name, a mode's raw
    /// value.
    pub value: String,
    /// What the reader sees.
    pub title: SharedString,
}

impl Choice {
    pub fn new(value: impl Into<String>, title: impl Into<SharedString>) -> Self {
        Self {
            value: value.into(),
            title: title.into(),
        }
    }
}

/// A picker's state: what it offers and what is in effect.
///
/// A picker draws nothing until something is in effect. A segmented control
/// with no selected segment claims the reader has chosen nothing, which is a
/// different statement from "the model has not loaded yet".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picker {
    pub choices: Vec<Choice>,
    pub selected: Option<String>,
}

impl Picker {
    pub fn new(choices: Vec<Choice>, selected: impl Into<String>) -> Self {
        Self {
            choices,
            selected: Some(selected.into()),
        }
    }

    /// Whether this picker has anything to draw.
    pub fn is_visible(&self) -> bool {
        !self.choices.is_empty() && self.selected.is_some()
    }
}

/// What the composer needs to know that it cannot derive from the field.
#[derive(Clone, Default)]
pub struct ComposerChrome {
    /// A turn is running: the send button becomes a stop button.
    pub busy: bool,
    /// Prompts queued behind the running turn, oldest first.
    ///
    /// Pass *every* queued prompt, not `Conversation::queued_previews`, when you
    /// want the overflow line to be truthful: previews are truncated to three
    /// by the engine, so a caller that passes them cannot tell the composer how
    /// many it is hiding.
    pub queued: Vec<String>,
    /// Whether the composer accepts input at all.
    pub enabled: bool,
    /// Whether attachment handling is available for this composer.
    pub attachments_enabled: bool,
    /// The provider model, drawn as a segmented control.
    pub model: Picker,
    /// The reasoning effort.
    pub effort: Picker,
    /// The conversation mode.
    pub mode: Picker,
    on_action: Option<ActionHandler>,
}

impl std::fmt::Debug for ComposerChrome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The handler is deliberately absent: a closure's Debug is either a
        // pointer or a mile of type names, and neither says anything about the
        // state a caller set.
        f.debug_struct("ComposerChrome")
            .field("busy", &self.busy)
            .field("queued", &self.queued)
            .field("enabled", &self.enabled)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("mode", &self.mode)
            .field("wired", &self.on_action.is_some())
            .finish()
    }
}

impl ComposerChrome {
    /// Report every control's activation through one handler.
    ///
    /// Absent a handler, every control here renders disabled.
    pub fn on_action(
        mut self,
        handler: impl Fn(ComposerAction, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_action = Some(Rc::new(handler));
        self
    }

    /// Whether Send would do anything: the composer is enabled, no turn is
    /// running, and there is a prompt to send.
    pub fn can_send(&self, draft: &str) -> bool {
        self.enabled && !self.busy && has_text(draft)
    }

    /// Whether the queue strip has anything to show.
    pub fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// The action handler, shared by every control that needs one.
    fn handler(&self) -> Option<ActionHandler> {
        self.on_action.clone()
    }
}

/// Whether the draft is a prompt rather than whitespace.
///
/// Trimmed rather than tested for emptiness: a field holding a single space is
/// the most common way a reader believes they have typed something and have
/// not, and sending it produces an empty turn rather than an error.
fn has_text(draft: &str) -> bool {
    !draft.trim().is_empty()
}

/// Opacity of a picker that cannot change the current selection.
///
/// The same decision `button.rs` makes for a disabled button, at the same
/// value. It is duplicated rather than shared because that constant is private
/// to the design system, and a view inventing a second one would drift from it.
const DISABLED_PICKER_OPACITY: f32 = 0.52;

/// The dot that marks a queued prompt, on the baseline grid rather than as a
/// literal: 8px minus the 2px step.
const QUEUE_DOT_SIZE: f32 = Spacing::SM - Spacing::XXXS;

/// Lookup keys for the two elements whose measured bounds a test asserts on.
///
/// "Looks right at a narrow width" is not otherwise checkable in GPUI 0.2.2:
/// there is no geometry a render function can read back, and this is the only
/// hook that records one. It compiles to a no-op outside test builds.
const CARD_SELECTOR: &str = "composer.card";
const PRIMARY_SELECTOR: &str = "composer.primary";

// ── Entry point ────────────────────────────────────────────────────────────

/// Render the composer around a caller-owned text field.
pub fn view(
    field: &Entity<TextField>,
    chrome: &mut ComposerChrome,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    // Two decisions taken outside the element tree: whether Send would do
    // anything, and whether the field has focus. Both are reads of the field,
    // and a view that re-read them per draw would answer with a different
    // answer on either side of a keystroke.
    let can_send = chrome.can_send(field.read(cx).value());
    // The only reason `window` is here. The field draws its own accent edge when
    // focused, so the card strengthens its *neutral* edge rather than going
    // accent too — two accent lines round one field read as two things.
    let focused = field.read(cx).focus_handle().is_focused(window);

    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .items_center()
        .bg(Colors::content_bg(&theme))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                // On the transcript's measure, so the card lines up with the
                // rows above it rather than running the width of the window.
                .max_w(px(Layout::READABLE_CONTENT_WIDTH))
                .px(px_len(Spacing::XXL))
                .pt(px_len(Spacing::SM))
                .pb(px_len(Spacing::XXL))
                .child(composer_card(field, chrome, focused, can_send)),
        )
        .into_any_element()
}

/// The bordered box the prompt and its controls sit in.
fn composer_card(
    field: &Entity<TextField>,
    chrome: &ComposerChrome,
    focused: bool,
    can_send: bool,
) -> AnyElement {
    let theme = global::current();
    // The card's edge tracks the field's focus, in the same neutral token pair
    // `TextField` uses for its own border: focused is the regular weight, at rest
    // is the hairline. Strengthening rather than tinting, because the accent
    // belongs to the field inside it and a second accent line round the same
    // field reads as two controls.
    let border = if focused {
        Colors::border(&theme)
    } else {
        Colors::border_subtle(&theme)
    };
    // Mirrors what the field does with its own fill: a composer that cannot be
    // typed into reads as part of the page rather than as an input.
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px_len(Radii::XL))
        .bg(if chrome.enabled {
            Colors::bg_surface(&theme)
        } else {
            Colors::bg(&theme)
        })
        .border(px_len(if focused {
            BorderWidth::REGULAR
        } else {
            BorderWidth::HAIRLINE
        }))
        .border_color(border)
        .debug_selector(|| CARD_SELECTOR.to_string())
        .p(px_len(Spacing::SM));

    if chrome.has_queued() {
        card = card.child(queued_strip(chrome));
    }

    card.child(
        div()
            .w_full()
            .pt(px_len(Spacing::XXXS))
            .pb(px_len(Spacing::SM))
            .child(field.clone().into_any_element()),
    )
    .child(control_row(chrome, can_send))
    .into_any_element()
}

/// The row of controls under the prompt, with the primary action at the end.
fn control_row(chrome: &ComposerChrome, can_send: bool) -> AnyElement {
    let handler = chrome.handler();
    // A picker cannot change anything mid-turn: the provider session it would
    // retarget is already committed to the running one, which is why the
    // original froze these controls while streaming rather than merely
    // greying them.
    let pickers_live = chrome.enabled && !chrome.busy;

    // The controls wrap; the row around them does not. The original scrolled
    // this row horizontally and faded its trailing edge, which needs a gradient
    // mask GPUI 0.2.2 cannot express — and a control the reader has to scroll to
    // find is the same failure as a queued prompt they cannot see. Wrapping
    // makes the composer taller instead, which costs nothing and keeps the
    // primary control on the same row as everything else.
    //
    // The wrap has to be on the element that *holds* the controls: a wrapping
    // container with one child cannot wrap, which is the same mistake as a
    // scroll container with one child.
    let mut controls = div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .flex_wrap()
        .gap(px_len(Spacing::SM));

    controls = controls.child(icon_button(
        IconButtonProps::new(Icon::PAPERCLIP, "Attach images")
            .small()
            .ghost()
            .enabled(chrome.enabled && chrome.attachments_enabled && handler.is_some()),
        fire(handler.clone(), ComposerAction::Attach),
    ));

    if let Some(model) =
        picker_control(&chrome.model, ComposerPicker::Model, &handler, pickers_live)
    {
        controls = controls.child(model).child(control_divider());
    }
    if let Some(effort) = picker_control(
        &chrome.effort,
        ComposerPicker::Effort,
        &handler,
        pickers_live,
    ) {
        controls = controls.child(effort).child(control_divider());
    }
    if let Some(mode) = picker_control(&chrome.mode, ComposerPicker::Mode, &handler, pickers_live) {
        controls = controls.child(mode);
    }

    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .child(controls)
        .child(primary_action(chrome, can_send))
        .into_any_element()
}

/// The one control whose meaning depends on the turn: Send, or Stop.
fn primary_action(chrome: &ComposerChrome, can_send: bool) -> AnyElement {
    let theme = global::current();
    let (icon, tint, enabled, label) = if chrome.busy {
        // Error, as in the original: this is the only destructive control on
        // screen, and it should read as one.
        (
            Icon::STOP_FILL,
            Colors::error(&theme),
            chrome.handler().is_some(),
            SharedString::from("Stop this turn"),
        )
    } else {
        (
            Icon::ARROW_UP,
            Colors::accent(&theme),
            can_send,
            SharedString::from("Send this prompt"),
        )
    };

    // Stop stays live in a disabled composer. A turn that cannot be interrupted
    // is the one failure mode with no way out of it, and the alternative — a
    // greyed-out stop on a running turn — has no way out of it either.
    // The ring draws an empty progress track: `ComposerChrome` carries no
    // context reading, so there is nothing to put in it yet.
    let progress = None;

    // The ring control owns its own element id, so the tooltip hangs off a
    // wrapper sized to it rather than off the ring itself. The wrapper is what
    // the narrow-width measurement looks at.
    div()
        .id("composer.primary")
        .flex_shrink_0()
        .debug_selector(|| PRIMARY_SELECTOR.to_string())
        .tooltip(move |window, cx| fx_design::components::tooltip(label.clone(), window, cx))
        .child(context_ring_icon(
            icon,
            progress,
            tint,
            ICON_BUTTON_SIZE,
            enabled,
            // No hover state: this is a function view with nothing to hold one
            // in, and the original's ring carries no hover tint of its own
            // either.
            false,
            fire(
                chrome.handler(),
                if chrome.busy {
                    ComposerAction::Stop
                } else {
                    ComposerAction::Submit
                },
            ),
        ))
        .into_any_element()
}

/// One picker, or nothing when it has nothing to show.
fn picker_control(
    picker: &Picker,
    which: ComposerPicker,
    handler: &Option<ActionHandler>,
    live: bool,
) -> Option<AnyElement> {
    if !picker.is_visible() {
        return None;
    }
    // The only way to reach a `segmented_control` is through a `&String`, so the
    // selection is borrowed rather than cloned. `is_visible` has already
    // established one exists; the `?` is here so that if it ever stops being
    // true this returns nothing rather than unwrapping inside a render.
    let selected = picker.selected.as_ref()?;
    let options: Vec<SegmentedOption<String>> = picker
        .choices
        .iter()
        .map(|choice| SegmentedOption::new(choice.value.clone(), choice.title.clone()))
        .collect();

    let on_select = handler.clone();
    let control = segmented_control(&options, selected, move |value, window, cx| {
        if live && let Some(on_select) = &on_select {
            on_select(ComposerAction::Select(which, value.clone()), window, cx);
        }
    });

    let mut element = div().flex_shrink_0();
    if !live || handler.is_none() {
        element = element.opacity(DISABLED_PICKER_OPACITY);
    }
    Some(element.child(control).into_any_element())
}

/// The rule between control groups.
///
/// `divider_with_axis` is the shared component, but its vertical rule is sized
/// `h_full`, and a percentage height resolves against an auto-height wrapping
/// row to nothing. The height is stated instead.
fn control_divider() -> AnyElement {
    let theme = global::current();
    div()
        .w(px_len(BorderWidth::REGULAR))
        .h(px_len(Spacing::LG))
        .flex_shrink_0()
        .bg(Colors::border_subtle(&theme))
        .into_any_element()
}

/// A click handler for one action, or one that does nothing.
///
/// An unwired composer still renders every control, disabled rather than absent,
/// so every handler has to cope with there being no handler to call.
fn fire(
    handler: Option<ActionHandler>,
    action: ComposerAction,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_event, window, cx| {
        if let Some(handler) = &handler {
            handler(action.clone(), window, cx);
        }
    }
}

// ── The queue ──────────────────────────────────────────────────────────────

/// The prompts waiting behind the running turn.
///
/// Not a transcript row, and not decorative: a queued prompt the reader cannot
/// see is a prompt that is silently not sent. It sits inside the card, above
/// the field, because that is the one place that is on screen while the reader
/// is looking at the transcript.
fn queued_strip(chrome: &ComposerChrome) -> AnyElement {
    if !chrome.has_queued() {
        return div().w(px(0.0)).h(px(0.0)).into_any_element();
    }
    let theme = global::current();
    let handler = chrome.handler();

    let mut rows = div().flex().flex_col().gap(px_len(Spacing::XXS));
    for (index, preview) in visible_queued(&chrome.queued).iter().enumerate() {
        rows = rows.child(queued_row(preview, index, &handler));
    }
    // Only stated when the caller handed over more than the strip shows; a
    // truncated `queued_previews` leaves nothing to count.
    let hidden = hidden_queued(&chrome.queued);
    if hidden > 0 {
        rows = rows.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .pl(px_len(Spacing::MD))
                .fx_caption()
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from(format!("+{hidden} more"))),
        );
    }

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .pb(px_len(Spacing::SM))
        .mb(px_len(Spacing::XXXS))
        .border_b_1()
        .border_color(Colors::border_subtle(&theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(Icon::HOURGLASS_BOTTOMHALF_FILLED.render(
                    IconSize::Small,
                    Colors::fg_secondary(&theme),
                    &theme,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .fx_role(TextRole::CaptionMedium)
                        .text_color(Colors::fg_secondary(&theme))
                        .child(SharedString::from(waiting_label(chrome.queued.len()))),
                )
                .child(badge(
                    SharedString::from("Queued"),
                    BadgeTone::Warning,
                    false,
                )),
        )
        .child(rows)
        .into_any_element()
}

/// One queued prompt: what it says, that it goes next, and a way to take it back.
fn queued_row(preview: &str, index: usize, handler: &Option<ActionHandler>) -> AnyElement {
    let theme = global::current();
    let is_next = index == 0;

    let mut row = div()
        .w_full()
        .flex()
        .flex_row()
        .items_start()
        .gap(px_len(Spacing::SM))
        // The first row gets the tint and the rest do not, so "which one goes
        // next" is readable without counting.
        .child(
            div()
                .mt(px_len(Spacing::XXS))
                .size(px_len(QUEUE_DOT_SIZE))
                .flex_shrink_0()
                .rounded_full()
                .bg(if is_next {
                    Colors::info(&theme)
                } else {
                    Colors::fg_quaternary(&theme)
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .fx_caption()
                .text_color(Colors::fg_secondary(&theme))
                // Two lines: a queued prompt is usually a sentence, and one line
                // of it is not enough to recognise before it is sent.
                .line_clamp(2)
                .child(SharedString::from(preview.to_string())),
        );

    if is_next {
        row = row.child(
            div()
                .flex_shrink_0()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from("next")),
        );
    }

    row.child(icon_button(
        IconButtonProps::new(
            Icon::XMARK,
            // The label is part of the element id, so it has to distinguish rows
            // or two queued prompts would share one control's hover state.
            format!("Remove queued prompt {}", index + 1),
        )
        .tiny()
        .ghost()
        .enabled(handler.is_some()),
        fire(handler.clone(), ComposerAction::RemoveQueued(index)),
    ))
    .into_any_element()
}

/// How many prompts the strip is standing in for.
fn waiting_label(count: usize) -> String {
    match count {
        1 => "1 prompt waiting".to_string(),
        other => format!("{other} prompts waiting"),
    }
}

/// The queued prompts the strip has room for.
///
/// Capped at the engine's own limit rather than a second one of its own: two
/// limits on one queue is one too many, and the copy is the one that would not
/// move when the other moved. Taking from the front is also what makes
/// [`ComposerAction::RemoveQueued`]'s index a real queue position.
fn visible_queued(queued: &[String]) -> &[String] {
    &queued[..queued.len().min(VISIBLE_QUEUED_PROMPTS)]
}

/// How many queued prompts the strip is not showing.
fn hidden_queued(queued: &[String]) -> usize {
    queued.len() - visible_queued(queued).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Context, Pixels, Render, TestAppContext, VisualTestContext, size};

    // The measured-bounds tests need the handle's inner window, which
    // `WindowHandle` only exposes through `Deref`.
    use std::ops::Deref;

    // ── Pure decisions ──────────────────────────────────────────────────────

    fn choice(value: &str, title: &'static str) -> Choice {
        Choice::new(value, title)
    }

    #[test]
    fn an_unwired_composer_can_do_nothing() {
        // Absent a handler every control renders disabled; this is the state a
        // caller gets before it has decided anything.
        let chrome = ComposerChrome::default();
        assert!(!chrome.enabled);
        assert!(!chrome.busy);
        assert!(chrome.queued.is_empty());
        assert!(chrome.handler().is_none());
        assert!(
            !chrome.can_send("a prompt"),
            "an unwired composer must not offer Send"
        );
        assert!(!chrome.can_send(""), "nor on an empty draft");
    }

    #[test]
    fn a_wired_but_disabled_composer_still_cannot_send() {
        // The `enabled` flag is the caller's say, not a consequence of having a
        // handler: a conversation whose provider died stays disabled.
        let chrome = ComposerChrome {
            enabled: false,
            ..ComposerChrome::default()
        }
        .on_action(|_, _, _| {});
        assert!(!chrome.can_send("a prompt"));
    }

    #[test]
    fn send_needs_text_and_an_idle_turn() {
        let chrome = ComposerChrome {
            enabled: true,
            ..ComposerChrome::default()
        };
        assert!(!chrome.can_send(""));
        assert!(!chrome.can_send("   \n\t "), "whitespace is not a prompt");
        assert!(chrome.can_send("a prompt"));
        assert!(chrome.can_send("  padded  "));
    }

    #[test]
    fn a_running_turn_never_offers_send() {
        // The same control is Stop while busy, and reporting Send as available
        // during a turn would be two answers to one button.
        let chrome = ComposerChrome {
            enabled: true,
            busy: true,
            ..ComposerChrome::default()
        };
        assert!(!chrome.can_send("a prompt"));
    }

    #[test]
    fn a_picker_draws_only_once_something_is_in_effect() {
        assert!(!Picker::default().is_visible());
        assert!(
            !Picker::new(Vec::new(), "gpt").is_visible(),
            "no choices is not a control"
        );
        assert!(
            !Picker {
                choices: vec![choice("gpt", "GPT")],
                selected: None,
            }
            .is_visible(),
            "a segmented control with nothing selected claims something untrue"
        );
        assert!(Picker::new(vec![choice("gpt", "GPT")], "gpt").is_visible());
    }

    #[test]
    fn a_default_picker_offers_nothing_to_choose_from() {
        let chrome = ComposerChrome::default();
        assert!(!chrome.model.is_visible());
        assert!(!chrome.effort.is_visible());
        assert!(!chrome.mode.is_visible());
    }

    #[test]
    fn the_queue_strip_says_how_many_prompts_are_waiting() {
        assert_eq!(waiting_label(1), "1 prompt waiting");
        assert_eq!(waiting_label(2), "2 prompts waiting");
        assert_eq!(waiting_label(11), "11 prompts waiting");
    }

    #[test]
    fn an_empty_queue_has_no_strip() {
        assert!(!ComposerChrome::default().has_queued());
        let one = ComposerChrome {
            queued: vec!["a prompt".into()],
            ..ComposerChrome::default()
        };
        assert!(one.has_queued());
    }

    #[test]
    fn the_strip_shows_the_front_of_the_queue_and_counts_the_rest() {
        let queued: Vec<String> = (0..VISIBLE_QUEUED_PROMPTS + 4)
            .map(|index| format!("queued prompt {index}"))
            .collect();

        // The cap is the engine's, so a strip that showed more would disagree
        // with `Conversation::queued_previews` about the same queue.
        assert_eq!(visible_queued(&queued).len(), VISIBLE_QUEUED_PROMPTS);
        assert_eq!(hidden_queued(&queued), 4);
        // Front first: the head of the strip is what goes next, and
        // `RemoveQueued`'s index is a real queue position because of it.
        assert_eq!(visible_queued(&queued)[0], "queued prompt 0");
        assert_eq!(visible_queued(&queued)[1], "queued prompt 1");
    }

    #[test]
    fn a_queue_the_strip_can_hold_hides_nothing() {
        let queued: Vec<String> = (0..VISIBLE_QUEUED_PROMPTS)
            .map(|index| format!("queued prompt {index}"))
            .collect();
        assert_eq!(visible_queued(&queued).len(), queued.len());
        assert_eq!(hidden_queued(&queued), 0);
        // An overflow line that appeared at one queue length but not another
        // would be the bug this is here for.
        assert_eq!(hidden_queued(&["only".to_string()]), 0);
        assert_eq!(hidden_queued(&[]), 0);
    }

    #[test]
    fn a_disabled_picker_recedes_like_a_disabled_button() {
        // The same value `button.rs` uses for a disabled button; a picker that
        // cannot change anything has to read as inert.
        const { assert!((DISABLED_PICKER_OPACITY - 0.52).abs() < f32::EPSILON) };
        const {
            assert!(
                DISABLED_PICKER_OPACITY > 0.0 && DISABLED_PICKER_OPACITY < 1.0,
                "a disabled control has to be dimmed, not hidden"
            )
        };
    }

    #[test]
    fn chrome_debug_omits_the_handler() {
        // A closure's Debug is either a pointer or a mile of type names; the
        // presence flag is the part that says anything.
        let chrome = ComposerChrome {
            enabled: true,
            ..ComposerChrome::default()
        }
        .on_action(|_, _, _| {});
        let rendered = format!("{chrome:?}");
        assert!(rendered.contains("wired: true"), "{rendered}");
        assert!(!format!("{:?}", ComposerChrome::default()).contains("wired: true"));
    }

    #[test]
    fn the_queue_dot_sits_on_the_baseline_grid() {
        assert_eq!(QUEUE_DOT_SIZE, 6.0);
        assert!(Spacing::ALL.contains(&QUEUE_DOT_SIZE));
    }

    // ── Rendering ───────────────────────────────────────────────────────────

    /// The smallest thing that can call `view` with a real window.
    struct Harness {
        field: Entity<TextField>,
        chrome: ComposerChrome,
        draft: String,
    }

    impl Harness {
        fn new(field: Entity<TextField>, chrome: ComposerChrome, draft: &str) -> Self {
            Self {
                field,
                chrome,
                draft: draft.to_string(),
            }
        }
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.field
                .update(cx, |field, cx| field.set_value(self.draft.clone(), cx));
            view(&self.field, &mut self.chrome, window, cx)
        }
    }

    /// Paints the composer once and insists it survived.
    ///
    /// A bare element cannot be painted here — the field registers a platform
    /// input handler while it draws, and that reaches for the view currently
    /// painting, which only a `Render` impl populates — so this mounts a
    /// one-line probe and paints that, the way the transcript tests do.
    fn paint(chrome: ComposerChrome, draft: &str) {
        let mut test = TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        let harness = Harness::new(field, chrome, draft);
        let window = test.add_window(move |_window, _cx| harness);
        test.refresh().expect("the composer painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    /// An enabled composer wired to a handler that records nothing.
    fn live() -> ComposerChrome {
        ComposerChrome {
            enabled: true,
            ..ComposerChrome::default()
        }
        .on_action(|_, _, _| {})
    }

    fn effort_picker() -> Picker {
        Picker::new(
            vec![
                choice("low", "Low"),
                choice("medium", "Medium"),
                choice("high", "High"),
            ],
            "medium",
        )
    }

    #[test]
    fn an_empty_composer_paints() {
        // The first thing a reader sees: a prompt box with nothing in it and a
        // Send that cannot work.
        paint(live(), "");
    }

    #[test]
    fn a_composer_with_a_draft_paints() {
        paint(live(), "make the thing");
    }

    #[test]
    fn a_composer_holding_only_whitespace_paints() {
        // The send control must read as unavailable, and nothing about the tree
        // should assume the draft is a real prompt.
        paint(live(), "   \n  ");
    }

    #[test]
    fn a_busy_composer_paints_its_stop_control() {
        paint(
            ComposerChrome {
                busy: true,
                ..live()
            },
            "",
        );
    }

    #[test]
    fn a_busy_composer_with_a_draft_paints() {
        // A draft typed mid-turn is queued rather than sent, which is the case
        // where the strip and the stop button both have to be on screen.
        paint(
            ComposerChrome {
                busy: true,
                queued: vec!["now finish the tests".into()],
                ..live()
            },
            "and the docs",
        );
    }

    #[test]
    fn a_disabled_composer_paints() {
        paint(ComposerChrome::default(), "a draft it will never send");
    }

    #[test]
    fn an_unwired_composer_paints_every_control_disabled() {
        // Nothing here has a handler, so nothing here may look live.
        paint(
            ComposerChrome {
                queued: vec!["waiting".into()],
                model: Picker::new(vec![choice("gpt-5", "GPT-5")], "gpt-5"),
                effort: effort_picker(),
                mode: Picker::new(vec![choice("auto", "Chat"), choice("plan", "Plan")], "auto"),
                ..ComposerChrome::default()
            },
            "",
        );
    }

    #[test]
    fn a_queued_prompt_paints_alone() {
        paint(
            ComposerChrome {
                queued: vec!["run the suite".into()],
                ..live()
            },
            "",
        );
    }

    #[test]
    fn more_queued_prompts_than_the_strip_shows_paint() {
        // The overflow line only appears when the caller handed over more than
        // the strip shows, so both halves of that have to survive a paint.
        let queued = (0..VISIBLE_QUEUED_PROMPTS + 4)
            .map(|index| format!("queued prompt {index}"))
            .collect();
        paint(
            ComposerChrome {
                busy: true,
                queued,
                ..live()
            },
            "",
        );
    }

    #[test]
    fn every_picker_paints_with_a_selection() {
        paint(
            ComposerChrome {
                model: Picker::new(
                    vec![choice("gpt-5", "GPT-5"), choice("claude", "Claude")],
                    "gpt-5",
                ),
                effort: effort_picker(),
                mode: Picker::new(vec![choice("auto", "Chat"), choice("plan", "Plan")], "plan"),
                ..live()
            },
            "",
        );
    }

    #[test]
    fn a_picker_with_no_selection_paints_without_it() {
        // Nothing is in effect yet, so the control is absent rather than drawn
        // with no segment filled.
        paint(
            ComposerChrome {
                effort: Picker {
                    choices: vec![choice("low", "Low"), choice("high", "High")],
                    selected: None,
                },
                ..live()
            },
            "",
        );
    }

    #[test]
    fn every_picker_paints_inert_while_a_turn_runs() {
        // The provider session a picker would retarget is already committed.
        paint(
            ComposerChrome {
                busy: true,
                effort: effort_picker(),
                ..live()
            },
            "a draft typed mid-turn",
        );
    }

    #[test]
    fn a_focused_composer_paints() {
        // Focus starts the field's caret task and registers the platform input
        // handler, neither of which any other test here reaches.
        let mut test = TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        let mounted = field.clone();
        let harness = Harness::new(field, live(), "focused draft");
        let window = test.add_window(move |_window, _cx| harness);
        window
            .update(&mut test, |_entity, window, cx| {
                mounted.read(cx).focus_handle().focus(window);
            })
            .expect("the window is live");
        test.refresh().expect("the focused composer painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    #[test]
    fn a_multiline_composer_paints_a_draft_long_enough_to_wrap() {
        // The width a wrapping field needs only exists once it has been laid out
        // once, so this is the case where the field's own measurement is wrong
        // on the first pass and correct on the second.
        let mut test = TestAppContext::single();
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).multiline(4.0)));
        let harness = Harness::new(
            field,
            live(),
            "first line\nsecond line, long enough that it has to wrap somewhere",
        );
        let window = test.add_window(move |_window, _cx| harness);
        test.refresh().expect("the composer painted");
        test.refresh()
            .expect("the composer repainted with a measured width");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    /// Measures the composer at a given window width and hands the two bounds to
    /// `check`.
    ///
    /// The card and the primary control are the two whose position is the whole
    /// of "does this look right": the card is what has to fit, and the primary
    /// control is what must stay reachable at its trailing edge however the
    /// controls wrap.
    fn at_width(width: f32, check: impl FnOnce(Bounds<Pixels>, Bounds<Pixels>)) {
        let (card, primary) = measured_at(width);
        check(card, primary);
    }

    /// The card's and the primary control's measured bounds at a window width.
    fn measured_at(width: f32) -> (Bounds<Pixels>, Bounds<Pixels>) {
        let mut test = TestAppContext::single();
        let field = test.update(|cx| cx.new(TextField::new));
        let window =
            test.add_window(move |_window, _cx| Harness::new(field, widest_chrome(), "a draft"));
        // Cloned rather than borrowed from `test`, so the resize and the
        // refresh can happen without fighting over one mutable borrow.
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(width), px(600.0)));
        test.refresh()
            .expect("the composer painted at its new width");
        (
            visual
                .debug_bounds(CARD_SELECTOR)
                .expect("the card recorded its bounds"),
            visual
                .debug_bounds(PRIMARY_SELECTOR)
                .expect("the primary control recorded its bounds"),
        )
    }

    /// The widest chrome a caller can produce: every control at once.
    fn widest_chrome() -> ComposerChrome {
        ComposerChrome {
            busy: true,
            queued: vec![
                "run the suite".into(),
                "then update the docs".into(),
                "and open a pull request".into(),
            ],
            model: Picker::new(
                vec![
                    choice("gpt-5", "GPT-5"),
                    choice("claude", "Claude Sonnet 4.5"),
                    choice("gemini", "Gemini 3 Pro"),
                ],
                "claude",
            ),
            effort: Picker::new(
                vec![
                    choice("low", "Low"),
                    choice("medium", "Medium"),
                    choice("high", "High"),
                    choice("xhigh", "XHigh"),
                ],
                "high",
            ),
            mode: Picker::new(vec![choice("auto", "Chat"), choice("plan", "Plan")], "auto"),
            ..live()
        }
    }

    #[test]
    fn the_composer_fits_a_narrow_column() {
        // The inspector open and the git panel closed is the narrowest the
        // centre column gets, and it is the width at which a control that
        // assumes a fixed width gets pushed out of the window.
        let narrow = Layout::MINIMUM_CONVERSATION_WIDTH;
        at_width(narrow, |card, _| {
            assert!(
                card.size.width <= px(narrow),
                "the card is {} wide in a {narrow} window",
                card.size.width
            );
        });
    }

    #[test]
    fn the_primary_control_stays_inside_the_card_at_every_width() {
        // Wrapping controls must not take the primary control with them off the
        // trailing edge: it is the one control that has to remain reachable.
        for width in [
            Layout::MINIMUM_BROWSER_PREVIEW_WIDTH,
            Layout::MINIMUM_CONVERSATION_WIDTH,
            640.0,
            Layout::READABLE_CONTENT_WIDTH,
            Layout::READABLE_CONTENT_WIDTH + 400.0,
        ] {
            at_width(width, |card, primary| {
                let right = card.origin.x + card.size.width;
                assert!(
                    primary.origin.x + primary.size.width <= right,
                    "at {width}: the primary control ends at {}, past the card's {right}",
                    primary.origin.x + primary.size.width
                );
                assert!(
                    primary.origin.x >= card.origin.x,
                    "at {width}: the primary control starts left of the card"
                );
            });
        }
    }

    #[test]
    fn the_controls_wrap_rather_than_running_off_the_edge() {
        // The alternative to wrapping was the original's horizontal scroll, and
        // the two are indistinguishable from outside except in one respect: a
        // wrapped row makes the composer taller. If that never happens, the row
        // is being clipped instead.
        let (wide, _) = measured_at(Layout::READABLE_CONTENT_WIDTH);
        let (narrow, _) = measured_at(Layout::MINIMUM_BROWSER_PREVIEW_WIDTH);
        assert!(
            narrow.size.height > wide.size.height,
            "the controls did not wrap: {} tall at {narrow} against {wide}",
            narrow.size.height
        );
    }

    #[test]
    fn a_wide_column_puts_the_primary_control_at_the_trailing_edge() {
        // The wrap is a fallback, not the normal case: on a comfortable width
        // the controls stay on one line and the primary control sits at the
        // trailing edge of the card.
        at_width(Layout::READABLE_CONTENT_WIDTH, |card, primary| {
            let inset = f32::from(card.origin.x + card.size.width)
                - f32::from(primary.origin.x + primary.size.width);
            // The card's own padding and hairline, and nothing else: any larger
            // gap means the control is not on this row's trailing edge at all.
            let expected = Spacing::SM + BorderWidth::HAIRLINE;
            assert!(
                (inset - expected).abs() < 1.0,
                "the primary control sits {inset}pt inside the card, not {expected}pt"
            );
        });
    }
}
