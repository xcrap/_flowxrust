//! The message composer: the prompt box, the controls under it, and the queue
//! above it.
//!
//! The text area is somebody else's — a [`TextField`] entity the caller owns and
//! hands in. Everything around it is here: the container, the primary action,
//! the model/effort controls, and the strip that shows prompts waiting
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
//! It also carries the two pickers' state and one callback.
//!
//! There is one callback, not one per control, because a method per callback is
//! a method per *decision*, and every decision here is the caller's. The caller
//! writes one match:
//!
//! ```ignore
//! let mut chrome = ComposerChrome {
//!     busy: conversation.is_busy(),
//!     queued: conversation.queued().map(QueuedPreview::of).collect(),
//!     enabled: true,
//!     model,
//!     effort,
//!     ..ComposerChrome::default()
//!     }
//!     .on_action(move |action, _window, cx| match action {
//!         ComposerAction::Submit => send(cx),
//!         ComposerAction::Stop => stop(cx),
//!         ComposerAction::Attach => attach(cx),
//!         ComposerAction::RemoveQueued(id) => dequeue(cx, id),
//!         ComposerAction::Select(ComposerPicker::Effort, value) => set_effort(cx, value),
//!         // Handle model selection in the caller as well.
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
//! # Images, steering and the queue
//!
//! The composer draws what the caller holds and reports what the reader did;
//! it prepares nothing itself. Images arrive three ways — the picker, a drop
//! on the card, and ⌘V with an image on the clipboard — and each is one
//! [`ComposerAction`]: reading a file and decoding a thumbnail are the
//! caller's and the asset loader's work, not a render's.
//!
//! While a turn runs, ⌘↩ and ⌃↩ submit as the two follow-ups — steer the
//! running turn, or queue behind it — in the order the caller's
//! [`FollowUp`] setting puts them. Both are actions bound in the composer's
//! key context, so they reach the composer and nothing else.
//!
//! # What the Swift did that this does not
//!
//! - **`focusComposer()` on appear.** A function view cannot tell a first render
//!   from a repaint, and focusing on every pass would pull the caret out of the
//!   terminal and the browser. The caller focuses the field.
//! - **Pasting a file copied in Finder.** GPUI's clipboard hands over text
//!   whenever the pasteboard has any, and a Finder copy carries the file's
//!   name as text, so it pastes the name. Dragging the file attaches it.

use std::path::PathBuf;
use std::rc::Rc;

use fx_agent::conversation::state::VISIBLE_QUEUED_PROMPTS;
use fx_core::models::{ConversationGoal, GoalStatus};
use fx_design::components::{
    BadgeTone, ButtonStyle, ContextUsage, IconButtonProps, ProviderIdentity, SegmentedOption,
    TurnAction, badge, button, context_meter, icon_button, segmented_control, turn_action_button,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, global};
use gpui::prelude::*;
use gpui::{AnyElement, App, ClickEvent, Entity, ExternalPaths, SharedString, Window, div, px};

use crate::state::FollowUp;
use crate::text_field::{self, TextField};

gpui::actions!(
    composer,
    [
        /// ⌘↩: send, or the default follow-up while a turn runs.
        SubmitPrompt,
        /// ⌃↩: send, or the other follow-up while a turn runs.
        SubmitAlternate,
        /// ⌘⇧A: choose images to attach to the active thread's prompt.
        AttachImages
    ]
);

/// The key context the composer's bindings are scoped to.
///
/// Scoped rather than global: ⌘↩ in a settings field must not send the
/// composer's draft. The queued-prompt editor sits inside this context and
/// claims both actions first, as a save.
pub const KEY_CONTEXT: &str = "Composer";

/// The callback every control reports through.
///
/// A `Window` and an `App`, because that is all a GPUI click hands a callback
/// and the state has to be reachable without being captured.
type ActionHandler = Rc<dyn Fn(ComposerAction, &mut Window, &mut App)>;

/// Which of the two pickers changed.
///
/// Named rather than stringly typed so the caller cannot silently misspell one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposerPicker {
    /// The provider model.
    Model,
    /// The reasoning effort.
    Effort,
}

/// Something a control asked for.
///
/// One enum rather than five callbacks: which controls exist is a layout
/// decision that can change, and a callback that survives the control being
/// removed is dead weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    /// Send the draft. Fired by the primary control when it is showing Send,
    /// and by ⌘↩, which while a turn runs means the default follow-up.
    Submit,
    /// ⌃↩: send, or the other follow-up while a turn runs.
    SubmitAlternate,
    /// Stop the running turn. Fired by the same control while it shows Stop.
    Stop,
    /// Open the attachment picker.
    Attach,
    /// Files were dropped on the composer.
    AttachPaths(Vec<PathBuf>),
    /// ⌘V found an image, not text, on the clipboard.
    PasteImage(gpui::Image),
    /// Take one pending image back out, by its attachment id.
    RemoveAttachment(uuid::Uuid),
    /// Open this queued prompt, by its id, for editing.
    EditQueued(uuid::Uuid),
    /// Save the open queued-prompt edit.
    SaveQueuedEdit,
    /// Close the queued-prompt editor without saving.
    CancelQueuedEdit,
    /// A picker changed. The value is the caller's own vocabulary, not the
    /// composer's.
    Select(ComposerPicker, String),
    /// Remove this queued prompt, by its id.
    ///
    /// An id rather than a position: the queue moves on its own when a turn
    /// ends, and a position read from the last frame then names the prompt
    /// behind the one the reader clicked.
    RemoveQueued(uuid::Uuid),
}

/// One queued prompt, as the strip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPreview {
    /// The prompt's own id, which is what the strip's controls act on.
    pub id: uuid::Uuid,
    pub text: String,
}

impl QueuedPreview {
    pub fn of(prompt: &fx_agent::conversation::QueuedPrompt) -> Self {
        Self {
            id: prompt.id,
            text: prompt.preview(),
        }
    }
}

/// A preview with an id of its own, for a composer that only displays.
impl From<&str> for QueuedPreview {
    fn from(text: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            text: text.to_string(),
        }
    }
}

impl From<String> for QueuedPreview {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

/// One exclusive choice in a picker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Choice {
    /// What the caller calls it — a model id or an effort name.
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
    /// Compact context gauge, separate from the send/stop action.
    pub context_usage: Option<ContextUsage>,
    pub provider: ProviderIdentity,
    /// A turn is running: the send button becomes a stop button.
    pub busy: bool,
    /// Prompts queued behind the running turn, oldest first.
    ///
    /// Pass *every* queued prompt, not `Conversation::queued_previews`, when you
    /// want the overflow line to be truthful: previews are truncated to three
    /// by the engine, so a caller that passes them cannot tell the composer how
    /// many it is hiding.
    pub queued: Vec<QueuedPreview>,
    /// Whether the composer accepts input at all.
    pub enabled: bool,
    /// Whether attachment handling is available for this composer.
    pub attachments_enabled: bool,
    /// The provider model, drawn as a segmented control.
    pub model: Picker,
    /// The reasoning effort.
    pub effort: Picker,
    /// Images waiting to go with the next prompt, in attachment order.
    pub attachments: Vec<ComposerAttachment>,
    /// Images are still being read, so Send would leave them behind.
    pub preparing_attachments: bool,
    /// What to tell the reader about their last attach, send, or steer.
    pub notice: Option<SharedString>,
    /// Guidance is waiting for the provider to take it. Nothing can be sent
    /// until it answers, because the draft is the guidance.
    pub steering: bool,
    /// What ⌘↩ does while a turn runs; ⌃↩ does the other.
    pub follow_up: FollowUp,
    /// The thread's goal, drawn above the card.
    pub goal: Option<ConversationGoal>,
    /// The queued prompt open for editing: its queue position, and its field.
    pub editing_queued: Option<(usize, Entity<TextField>)>,
    on_action: Option<ActionHandler>,
}

/// One image in the composer, as the chip row needs it.
#[derive(Clone)]
pub struct ComposerAttachment {
    pub id: uuid::Uuid,
    pub filename: SharedString,
    /// Interned through the inline-image cache, so a repaint neither copies
    /// nor re-hashes the image.
    pub(crate) image: crate::attachments::ImageInput,
}

impl ComposerAttachment {
    pub(crate) fn new(attachment: &fx_core::Attachment) -> Self {
        Self {
            id: attachment.id,
            filename: SharedString::from(attachment.filename.clone()),
            image: crate::attachments::ImageInput::inline(&attachment.data),
        }
    }
}

impl std::fmt::Debug for ComposerAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComposerAttachment")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .finish()
    }
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
            .field("attachments", &self.attachments)
            .field("steering", &self.steering)
            .field("follow_up", &self.follow_up)
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
    /// running, and there is a prompt to send — words, images, or both.
    ///
    /// Not while images are still being read: sending then would leave them
    /// behind, and the reader attached them to go with this prompt.
    pub fn can_send(&self, draft: &str) -> bool {
        self.enabled
            && !self.busy
            && !self.preparing_attachments
            && (has_text(draft) || !self.attachments.is_empty())
    }

    /// Whether ⌘↩ or ⌃↩ would do anything while a turn runs.
    pub fn can_follow_up(&self, draft: &str) -> bool {
        self.busy
            && !self.steering
            && !self.preparing_attachments
            && (has_text(draft) || !self.attachments.is_empty())
    }

    /// The keyboard hint under the card, which says what the keys do now.
    pub fn shortcut_hint(&self) -> &'static str {
        if self.steering {
            return "Sending guidance to the running turn…";
        }
        if !self.busy {
            return "⌘ Return to send · Return for a new line";
        }
        match self.follow_up {
            FollowUp::Steer => "⌘ Return to steer · ⌃ Return to queue",
            FollowUp::Queue => "⌘ Return to queue · ⌃ Return to steer",
        }
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

/// Whether the queued prompt's editor is the field that has focus.
fn editing_here(editor: Option<&Entity<TextField>>, window: &Window, cx: &App) -> bool {
    editor.is_some_and(|editor| editor.read(cx).focus_handle().is_focused(window))
}

/// Render the composer around a caller-owned text field.
pub fn view(
    field: &Entity<TextField>,
    chrome: &mut ComposerChrome,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    view_with_controls(field, chrome, None, window, cx)
}

/// The app supplies its thread configuration inside the same input surface.
pub fn view_with_controls(
    field: &Entity<TextField>,
    chrome: &mut ComposerChrome,
    configuration: Option<AnyElement>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = global::current();
    // Two decisions taken outside the element tree: whether Send would do
    // anything, and whether the field has focus. Both are reads of the field,
    // and a view that re-read them per draw would answer with a different
    // answer on either side of a keystroke.
    let can_send = chrome.can_send(field.read(cx).value());
    // The outer card owns focus styling for the app's borderless input.
    let focused = field.read(cx).focus_handle().is_focused(window);
    let handler = chrome.handler();
    let for_submit = handler.clone();
    let for_alternate = handler.clone();
    // An image on the clipboard is only an attachment where one can attach.
    let for_paste = handler
        .clone()
        .filter(|_| chrome.attachments_enabled && chrome.enabled && !chrome.steering);
    let for_paste_key = for_paste.clone();
    // A queued prompt open for editing takes its own paste. Caught here, an
    // image pasted there was attached to the draft below it instead.
    let editor = chrome
        .editing_queued
        .as_ref()
        .map(|(_, editor)| editor.clone());
    let editor_for_key = editor.clone();

    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .items_center()
        .bg(Colors::content_bg(&theme))
        .key_context(KEY_CONTEXT)
        .on_action(move |_: &SubmitPrompt, window, cx| {
            if let Some(handler) = &for_submit {
                handler(ComposerAction::Submit, window, cx);
            }
        })
        .on_action(move |_: &SubmitAlternate, window, cx| {
            if let Some(handler) = &for_alternate {
                handler(ComposerAction::SubmitAlternate, window, cx);
            }
        })
        // An image on the clipboard is an attachment, and the field would
        // otherwise paste nothing. Caught as the action, on its way down to
        // the field: GPUI runs a bound key's action before any key-down
        // listener and stops there, so with ⌘V bound — as the app binds it —
        // a key-down check never ran. The Edit menu's Paste arrives here too.
        .capture_action(move |_: &text_field::Paste, window, cx| {
            if let Some(handler) = &for_paste
                && !editing_here(editor.as_ref(), window, cx)
                && let Some(image) = clipboard_image(cx)
            {
                cx.stop_propagation();
                handler(ComposerAction::PasteImage(image), window, cx);
            }
        })
        // The same, for a window that binds nothing, where ⌘V reaches the
        // field as a key.
        .capture_key_down(move |event, window, cx| {
            if let Some(handler) = &for_paste_key
                && is_paste_key(&event.keystroke)
                && !editing_here(editor_for_key.as_ref(), window, cx)
                && let Some(image) = clipboard_image(cx)
            {
                cx.stop_propagation();
                handler(ComposerAction::PasteImage(image), window, cx);
            }
        })
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                // On the transcript's measure, so the card lines up with the
                // rows above it rather than running the width of the window.
                .max_w(px(Layout::READABLE_CONTENT_WIDTH))
                .px(px_len(Spacing::LG))
                .pt(px_len(Spacing::SM))
                .pb(px_len(Spacing::MD))
                .children(chrome.goal.as_ref().map(goal_row))
                .child(composer_card(
                    field,
                    chrome,
                    focused,
                    can_send,
                    configuration,
                ))
                .child(
                    div()
                        .pt(px_len(Spacing::SM))
                        .fx_caption()
                        .text_color(Colors::fg_tertiary(&theme))
                        .child(chrome.shortcut_hint()),
                ),
        )
        .into_any_element()
}

/// Whether a key is ⌘V.
fn is_paste_key(keystroke: &gpui::Keystroke) -> bool {
    keystroke.modifiers.secondary()
        && !keystroke.modifiers.shift
        && keystroke.key.eq_ignore_ascii_case("v")
}

/// The image a paste would insert, when the clipboard holds one and no text.
///
/// GPUI returns text whenever the pasteboard has any, so an image here is
/// what the reader copied rather than a rendition beside their words.
fn clipboard_image(cx: &App) -> Option<gpui::Image> {
    cx.read_from_clipboard()?
        .into_entries()
        .find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image),
            gpui::ClipboardEntry::String(_) => None,
        })
}

// ── The goal ───────────────────────────────────────────────────────────────

/// The thread's goal, above the card: what it is and where it stands.
fn goal_row(goal: &ConversationGoal) -> AnyElement {
    let theme = global::current();
    div()
        .debug_selector(|| "composer.goal".into())
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .mb(px_len(Spacing::SM))
        .px(px_len(Spacing::SM))
        .py(px_len(Spacing::XS))
        .rounded(px_len(Radii::SM))
        .bg(Colors::bg_surface(&theme))
        .child(Icon::TARGET.render(IconSize::Regular, Colors::accent(&theme), &theme))
        .child(
            div()
                .flex_shrink_0()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child("Goal"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .fx_caption()
                .text_color(Colors::fg(&theme))
                .child(SharedString::from(goal.objective.clone())),
        )
        .children(goal.usage_text().map(|usage| {
            div()
                .flex_shrink_0()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from(usage))
        }))
        .child(badge(goal.status.label(), goal_tone(goal.status), false))
        .into_any_element()
}

/// The original's tones: working is success, waiting is a warning, stuck is
/// an error, and done is information.
fn goal_tone(status: GoalStatus) -> BadgeTone {
    match status {
        GoalStatus::Active => BadgeTone::Success,
        GoalStatus::Paused => BadgeTone::Warning,
        GoalStatus::Blocked | GoalStatus::UsageLimited | GoalStatus::BudgetLimited => {
            BadgeTone::Error
        }
        GoalStatus::Complete => BadgeTone::Info,
    }
}

/// The bordered box the prompt and its controls sit in.
fn composer_card(
    field: &Entity<TextField>,
    chrome: &ComposerChrome,
    focused: bool,
    can_send: bool,
    configuration: Option<AnyElement>,
) -> AnyElement {
    let theme = global::current();
    // Keep the input and its controls on one surface with a single focus edge.
    let border = if focused {
        Colors::border(&theme)
    } else {
        Colors::border_subtle(&theme)
    };
    let accepts_drops = chrome.attachments_enabled && chrome.enabled && !chrome.steering;
    let on_drop = chrome.handler().filter(|_| accepts_drops);
    let mut card = div()
        .w_full()
        .flex()
        .flex_col()
        .rounded(px_len(Radii::XL))
        .bg(Colors::composer_bg(&theme))
        .border(px_len(BorderWidth::REGULAR))
        .border_color(border)
        .debug_selector(|| CARD_SELECTOR.to_string())
        .p(px_len(Spacing::MD));
    if let Some(on_drop) = on_drop {
        // The edge says the drop will land here, as the original's did.
        let accent = Colors::accent(&theme);
        card = card
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(accent))
            .on_drop(move |paths: &ExternalPaths, window, cx| {
                on_drop(
                    ComposerAction::AttachPaths(paths.paths().to_vec()),
                    window,
                    cx,
                );
            });
    }

    if chrome.has_queued() {
        card = card.child(queued_strip(chrome));
    }

    card.child(
        div()
            .w_full()
            .px(px_len(Spacing::XXS))
            .pt(px_len(Spacing::XXS))
            .min_h(px(Layout::COMPOSER_MIN_HEIGHT + Spacing::LG))
            .pb(px_len(Spacing::SM))
            .child(field.clone().into_any_element()),
    )
    .children(attachment_row(chrome))
    .children(chrome.notice.clone().map(|notice| {
        div()
            .debug_selector(|| "composer.notice".into())
            .w_full()
            .px(px_len(Spacing::XXS))
            .pb(px_len(Spacing::SM))
            .fx_caption()
            .text_color(Colors::fg_secondary(&theme))
            .child(notice)
    }))
    .child(control_row(chrome, can_send, configuration))
    .into_any_element()
}

/// The pending images, and a note while more are being read.
fn attachment_row(chrome: &ComposerChrome) -> Option<AnyElement> {
    if chrome.attachments.is_empty() && !chrome.preparing_attachments {
        return None;
    }
    let theme = global::current();
    let handler = chrome.handler();
    // Frozen while guidance is in flight: the images are part of it.
    let removable = !chrome.steering;
    let mut row = div()
        .debug_selector(|| "composer.attachments".into())
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px_len(Spacing::SM))
        .px(px_len(Spacing::XXS))
        .pb(px_len(Spacing::SM));
    for (index, attachment) in chrome.attachments.iter().enumerate() {
        let on_remove: crate::attachments::RemoveHandler = {
            let handler = handler.clone().filter(|_| removable);
            let id = attachment.id;
            Rc::new(move |window, cx| {
                if let Some(handler) = &handler {
                    handler(ComposerAction::RemoveAttachment(id), window, cx);
                }
            })
        };
        row = row.child(crate::attachments::pending_chip(
            attachment.image.clone(),
            // Part of the remove control's id, so two pastes named alike
            // still get a control each.
            format!("Remove image {} ({})", index + 1, attachment.filename),
            on_remove,
        ));
    }
    if chrome.preparing_attachments {
        row = row.child(
            div()
                .fx_caption()
                .text_color(Colors::fg_tertiary(&theme))
                .child("Preparing images…"),
        );
    }
    Some(row.into_any_element())
}

/// The row of controls under the prompt, with the primary action at the end.
fn control_row(
    chrome: &ComposerChrome,
    can_send: bool,
    configuration: Option<AnyElement>,
) -> AnyElement {
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

    if chrome.attachments_enabled {
        controls = controls.child(icon_button(
            IconButtonProps::new(Icon::PAPERCLIP, "Attach images")
                .small()
                .ghost()
                .enabled(
                    chrome.enabled
                        && chrome.attachments_enabled
                        && !chrome.steering
                        && handler.is_some(),
                ),
            fire(handler.clone(), ComposerAction::Attach),
        ));
    }

    if let Some(configuration) = configuration {
        controls = controls.child(configuration);
    }

    if let Some(model) =
        picker_control(&chrome.model, ComposerPicker::Model, &handler, pickers_live)
    {
        controls = controls.child(model);
        if chrome.effort.is_visible() {
            controls = controls.child(control_divider());
        }
    }
    if let Some(effort) = picker_control(
        &chrome.effort,
        ComposerPicker::Effort,
        &handler,
        pickers_live,
    ) {
        controls = controls.child(effort);
    }

    div()
        .w_full()
        .flex()
        .flex_row()
        .items_end()
        .gap(px_len(Spacing::SM))
        .child(controls)
        // Only a known percentage is shown. A meter that does not know
        // drew an empty ring and a dash, which read as a stray line.
        .children(
            chrome
                .context_usage
                .filter(|usage| usage.fraction().is_some())
                .map(|usage| context_meter(usage, chrome.provider)),
        )
        .child(primary_action(chrome, can_send))
        .into_any_element()
}

/// The one control whose meaning depends on the turn: Send, or Stop.
fn primary_action(chrome: &ComposerChrome, can_send: bool) -> AnyElement {
    let (action, enabled) = if chrome.busy {
        (TurnAction::Stop, chrome.handler().is_some())
    } else {
        (TurnAction::Send, can_send && chrome.handler().is_some())
    };

    // Stop stays live in a disabled composer. A turn that cannot be interrupted
    // is the one failure mode with no way out of it, and the alternative — a
    // greyed-out stop on a running turn — has no way out of it either.
    div()
        .id("composer.primary")
        .flex_shrink_0()
        .debug_selector(|| PRIMARY_SELECTOR.to_string())
        .child(turn_action_button(
            "composer.turn-action",
            action,
            enabled,
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
        if let Some((editing, field)) = &chrome.editing_queued
            && *editing == index
        {
            rows = rows.child(queued_editor(field, &handler));
        }
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
fn queued_row(
    preview: &QueuedPreview,
    index: usize,
    handler: &Option<ActionHandler>,
) -> AnyElement {
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
                .child(SharedString::from(preview.text.clone())),
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
        IconButtonProps::new(Icon::PENCIL, format!("Edit queued prompt {}", index + 1))
            .tiny()
            .ghost()
            .enabled(handler.is_some()),
        fire(handler.clone(), ComposerAction::EditQueued(preview.id)),
    ))
    .child(icon_button(
        IconButtonProps::new(
            Icon::XMARK,
            // The label is part of the element id, so it has to distinguish rows
            // or two queued prompts would share one control's hover state.
            format!("Remove queued prompt {}", index + 1),
        )
        .tiny()
        .ghost()
        .enabled(handler.is_some()),
        fire(handler.clone(), ComposerAction::RemoveQueued(preview.id)),
    ))
    .into_any_element()
}

/// The open edit of one queued prompt: its own field, then Cancel and Save.
///
/// The field sits inside the composer's key context, so ⌘↩ and ⌃↩ are
/// claimed here first and mean Save; Escape means Cancel.
fn queued_editor(field: &Entity<TextField>, handler: &Option<ActionHandler>) -> AnyElement {
    let theme = global::current();
    let for_submit = handler.clone();
    let for_alternate = handler.clone();
    let for_escape = handler.clone();
    div()
        .debug_selector(|| "composer.queue-editor".into())
        .w_full()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .pl(px_len(Spacing::LG))
        .on_action(move |_: &SubmitPrompt, window, cx| {
            if let Some(handler) = &for_submit {
                handler(ComposerAction::SaveQueuedEdit, window, cx);
            }
        })
        .on_action(move |_: &SubmitAlternate, window, cx| {
            if let Some(handler) = &for_alternate {
                handler(ComposerAction::SaveQueuedEdit, window, cx);
            }
        })
        .on_key_down(move |event, window, cx| {
            if event.keystroke.key == "escape"
                && let Some(handler) = &for_escape
            {
                cx.stop_propagation();
                handler(ComposerAction::CancelQueuedEdit, window, cx);
            }
        })
        .child(
            div()
                .w_full()
                .px(px_len(Spacing::MD))
                .py(px_len(Spacing::SM))
                .rounded(px_len(Radii::MD))
                .bg(Colors::bg_surface(&theme))
                .border(px_len(BorderWidth::HAIRLINE))
                .border_color(Colors::border_subtle(&theme))
                .child(field.clone().into_any_element()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px_len(Spacing::SM))
                .child(button(
                    "Cancel",
                    ButtonStyle::Ghost,
                    handler.is_some(),
                    fire(handler.clone(), ComposerAction::CancelQueuedEdit),
                ))
                .child(button(
                    "Save",
                    ButtonStyle::Primary,
                    handler.is_some(),
                    fire(handler.clone(), ComposerAction::SaveQueuedEdit),
                )),
        )
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
fn visible_queued(queued: &[QueuedPreview]) -> &[QueuedPreview] {
    &queued[..queued.len().min(VISIBLE_QUEUED_PROMPTS)]
}

/// How many queued prompts the strip is not showing.
fn hidden_queued(queued: &[QueuedPreview]) -> usize {
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
        let queued: Vec<QueuedPreview> = (0..VISIBLE_QUEUED_PROMPTS + 4)
            .map(|index| format!("queued prompt {index}").into())
            .collect();

        // The cap is the engine's, so a strip that showed more would disagree
        // with `Conversation::queued_previews` about the same queue.
        assert_eq!(visible_queued(&queued).len(), VISIBLE_QUEUED_PROMPTS);
        assert_eq!(hidden_queued(&queued), 4);
        // Front first: the head of the strip is what goes next.
        assert_eq!(visible_queued(&queued)[0].text, "queued prompt 0");
        assert_eq!(visible_queued(&queued)[1].text, "queued prompt 1");
    }

    #[test]
    fn a_queue_the_strip_can_hold_hides_nothing() {
        let queued: Vec<QueuedPreview> = (0..VISIBLE_QUEUED_PROMPTS)
            .map(|index| format!("queued prompt {index}").into())
            .collect();
        assert_eq!(visible_queued(&queued).len(), queued.len());
        assert_eq!(hidden_queued(&queued), 0);
        // An overflow line that appeared at one queue length but not another
        // would be the bug this is here for.
        assert_eq!(hidden_queued(&["only".into()]), 0);
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

    fn pending(fill: u8) -> ComposerAttachment {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(32, fill);
        ComposerAttachment::new(&fx_core::Attachment::new(
            data,
            "image/png",
            "Pasted Image.png",
        ))
    }

    #[test]
    fn images_alone_are_a_prompt_but_not_while_more_are_being_read() {
        let mut chrome = ComposerChrome {
            enabled: true,
            attachments: vec![pending(1)],
            ..ComposerChrome::default()
        };
        assert!(chrome.can_send(""), "an image with no words is a prompt");
        assert!(chrome.can_send("  \n"));
        chrome.preparing_attachments = true;
        assert!(
            !chrome.can_send("and words"),
            "sending now would leave the images being read behind"
        );
    }

    #[test]
    fn follow_ups_need_a_running_turn_and_wait_for_unanswered_guidance() {
        let mut chrome = ComposerChrome {
            enabled: true,
            ..ComposerChrome::default()
        };
        assert!(!chrome.can_follow_up("guide"), "nothing is running");
        chrome.busy = true;
        assert!(chrome.can_follow_up("guide"));
        assert!(!chrome.can_follow_up(" "));
        chrome.steering = true;
        assert!(
            !chrome.can_follow_up("guide"),
            "the draft is still the guidance"
        );
    }

    #[test]
    fn the_hint_says_what_the_keys_do_now() {
        let mut chrome = ComposerChrome::default();
        assert_eq!(
            chrome.shortcut_hint(),
            "⌘ Return to send · Return for a new line"
        );
        chrome.busy = true;
        assert_eq!(
            chrome.shortcut_hint(),
            "⌘ Return to steer · ⌃ Return to queue"
        );
        chrome.follow_up = FollowUp::Queue;
        assert_eq!(
            chrome.shortcut_hint(),
            "⌘ Return to queue · ⌃ Return to steer"
        );
        chrome.steering = true;
        assert!(chrome.shortcut_hint().contains("guidance"));
    }

    #[test]
    fn a_goal_badge_reads_as_the_original_s() {
        assert_eq!(goal_tone(GoalStatus::Active), BadgeTone::Success);
        assert_eq!(goal_tone(GoalStatus::Paused), BadgeTone::Warning);
        assert_eq!(goal_tone(GoalStatus::BudgetLimited), BadgeTone::Error);
        assert_eq!(goal_tone(GoalStatus::Complete), BadgeTone::Info);
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

    #[test]
    fn the_context_meter_appears_only_once_it_has_a_percentage_to_show() {
        // An unknown meter drew an empty ring and a dash, which read as a
        // stray line beside the send button.
        let meter_shown = |usage: ContextUsage| {
            let mut test = TestAppContext::single();
            let field = test.update(|cx| cx.new(TextField::new));
            let chrome = ComposerChrome {
                context_usage: Some(usage),
                ..live()
            };
            let window = test.add_window(move |_, _| Harness::new(field, chrome, ""));
            test.refresh().expect("the composer painted");
            let mut visual = VisualTestContext::from_window(*window.deref(), &test);
            visual.debug_bounds("composer-context").is_some()
        };
        let limit = Some(200_000);
        assert!(!meter_shown(ContextUsage { used: None, limit }));
        assert!(!meter_shown(ContextUsage {
            used: Some(5_000),
            limit: None
        }));
        assert!(meter_shown(ContextUsage {
            used: Some(5_000),
            limit
        }));
        assert!(meter_shown(ContextUsage {
            used: Some(0),
            limit
        }));
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
    fn send_and_stop_share_a_generous_target_and_stop_stays_live_during_transition() {
        use std::cell::RefCell;

        use gpui::{Modifiers, MouseButton};

        let mut test = TestAppContext::single();
        let actions = Rc::new(RefCell::new(Vec::new()));
        let recorded = actions.clone();
        let chrome = live().on_action(move |action, _, _| recorded.borrow_mut().push(action));
        let field = test.update(|cx| cx.new(TextField::new));
        let window = test.add_window(move |_, _| Harness::new(field, chrome, "a draft"));
        test.refresh().expect("send paints");
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        let send = visual.debug_bounds(PRIMARY_SELECTOR).expect("send target");
        assert_eq!(
            send.size,
            size(
                px(Layout::COMPOSER_ACTION_SIZE),
                px(Layout::COMPOSER_ACTION_SIZE)
            )
        );
        visual.simulate_mouse_move(send.center(), None, Modifiers::default());
        test.refresh().expect("hover transition paints");
        visual.simulate_mouse_down(send.center(), MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_up(send.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(*actions.borrow(), vec![ComposerAction::Submit]);

        window
            .update(&mut test, |harness, _, cx| {
                // Streaming can disable input, but must leave Stop reachable.
                harness.chrome.busy = true;
                harness.chrome.enabled = false;
                cx.notify();
            })
            .expect("the turn starts");
        test.refresh().expect("send fades into stop");
        let stop = visual.debug_bounds(PRIMARY_SELECTOR).expect("stop target");
        let glyph = visual
            .debug_bounds("turn-action-stop-glyph")
            .expect("solid stop glyph");
        assert_eq!(
            send, stop,
            "the control must not shift when its action changes"
        );
        assert_eq!(glyph.size, size(px(Spacing::MD), px(Spacing::MD)));
        assert_eq!(glyph.center(), stop.center());
        visual.simulate_mouse_down(stop.center(), MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_up(stop.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(
            *actions.borrow(),
            vec![ComposerAction::Submit, ComposerAction::Stop]
        );

        window
            .update(&mut test, |harness, _, cx| {
                harness.chrome.busy = false;
                cx.notify();
            })
            .expect("the turn finishes with input still disabled");
        test.refresh().expect("disabled send paints");
        visual.simulate_mouse_down(stop.center(), MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_up(stop.center(), MouseButton::Left, Modifiers::default());
        assert_eq!(actions.borrow().len(), 2, "disabled send is inert");
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
            .map(|index| format!("queued prompt {index}").into())
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

    #[test]
    fn a_composer_with_images_a_note_a_goal_and_an_open_edit_paints() {
        let mut test = TestAppContext::single();
        let (field, editor) = test.update(|cx| {
            (
                cx.new(TextField::new),
                cx.new(|cx| TextField::new(cx).multiline(4.0)),
            )
        });
        let goal = ConversationGoal::from_codex(&serde_json::json!({
            "threadId": "t", "objective": "Ship the release", "status": "paused",
            "tokensUsed": 1200, "tokenBudget": 5000
        }))
        .unwrap();
        let chrome = ComposerChrome {
            busy: true,
            queued: vec!["run the suite".into(), "then the docs".into()],
            editing_queued: Some((1, editor)),
            attachments: vec![pending(1), pending(2)],
            preparing_attachments: true,
            notice: Some("b.png is not a supported image.".into()),
            attachments_enabled: true,
            goal: Some(goal),
            ..live()
        };
        let window = test.add_window(move |_, _| Harness::new(field, chrome, "with images"));
        test.refresh().expect("the composer painted");
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        assert!(visual.debug_bounds("composer.goal").is_some());
        assert!(visual.debug_bounds("composer.attachments").is_some());
        assert!(visual.debug_bounds("composer.notice").is_some());
        assert!(visual.debug_bounds("composer.queue-editor").is_some());
        let chip = visual.debug_bounds("attachment.chip").expect("a chip");
        assert_eq!(
            chip.size,
            size(
                px(fx_design::components::attachment::ATTACHMENT_CHIP_SIZE),
                px(fx_design::components::attachment::ATTACHMENT_CHIP_SIZE)
            )
        );
    }

    /// Every action a composer reported, in order.
    type Recorded = Rc<std::cell::RefCell<Vec<ComposerAction>>>;

    /// A composer wired to record every action, with the app's bindings.
    fn recording(
        chrome: ComposerChrome,
        draft: &str,
    ) -> (
        TestAppContext,
        gpui::WindowHandle<Harness>,
        Entity<TextField>,
        Recorded,
    ) {
        let mut test = TestAppContext::single();
        test.update(|cx| {
            cx.bind_keys([
                gpui::KeyBinding::new("cmd-enter", SubmitPrompt, Some(KEY_CONTEXT)),
                gpui::KeyBinding::new("ctrl-enter", SubmitAlternate, Some(KEY_CONTEXT)),
            ])
        });
        let actions = Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = actions.clone();
        let chrome = chrome.on_action(move |action, _, _| recorded.borrow_mut().push(action));
        let field = test.update(|cx| cx.new(|cx| TextField::new(cx).multiline(4.0)));
        let mounted = field.clone();
        let draft = draft.to_string();
        let window = test.add_window(move |_, _| Harness::new(mounted, chrome, &draft));
        test.refresh().expect("the composer painted");
        (test, window, field, actions)
    }

    fn focus(
        test: &mut TestAppContext,
        window: &gpui::WindowHandle<Harness>,
        field: &Entity<TextField>,
    ) {
        let field = field.clone();
        window
            .update(test, |_, window, cx| {
                field.read(cx).focus_handle().focus(window)
            })
            .expect("the window is live");
        test.refresh().expect("focus painted");
    }

    #[test]
    fn command_and_control_return_are_the_two_follow_ups() {
        let (mut test, window, field, actions) = recording(
            ComposerChrome {
                busy: true,
                ..live()
            },
            "guide it",
        );
        focus(&mut test, &window, &field);
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_keystrokes("cmd-enter");
        visual.simulate_keystrokes("ctrl-enter");
        assert_eq!(
            *actions.borrow(),
            vec![ComposerAction::Submit, ComposerAction::SubmitAlternate]
        );
    }

    #[test]
    fn command_return_in_a_queued_prompt_s_editor_saves_the_edit() {
        let mut test = TestAppContext::single();
        test.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "cmd-enter",
                SubmitPrompt,
                Some(KEY_CONTEXT),
            )])
        });
        let actions = Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = actions.clone();
        let (field, editor) = test.update(|cx| {
            (
                cx.new(TextField::new),
                cx.new(|cx| TextField::new(cx).multiline(4.0)),
            )
        });
        let chrome = ComposerChrome {
            busy: true,
            queued: vec!["queued".into()],
            editing_queued: Some((0, editor.clone())),
            ..live()
        }
        .on_action(move |action, _, _| recorded.borrow_mut().push(action));
        let window = test.add_window(move |_, _| Harness::new(field, chrome, ""));
        test.refresh().expect("painted");
        let focused = editor.clone();
        window
            .update(&mut test, |_, window, cx| {
                focused.read(cx).focus_handle().focus(window)
            })
            .unwrap();
        test.refresh().unwrap();
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_keystrokes("cmd-enter");
        visual.simulate_keystrokes("escape");
        assert_eq!(
            *actions.borrow(),
            vec![
                ComposerAction::SaveQueuedEdit,
                ComposerAction::CancelQueuedEdit
            ],
            "the edit, not the composer's draft, is what ⌘↩ sends here"
        );
    }

    #[test]
    fn pasting_an_image_attaches_it_and_pasting_text_does_not() {
        let image =
            gpui::Image::from_bytes(gpui::ImageFormat::Png, b"\x89PNG\r\n\x1a\nrest".to_vec());
        let (mut test, window, field, actions) = recording(
            ComposerChrome {
                attachments_enabled: true,
                ..live()
            },
            "",
        );
        focus(&mut test, &window, &field);
        test.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image)));
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_keystrokes("cmd-v");
        assert_eq!(*actions.borrow(), vec![ComposerAction::PasteImage(image)]);

        test.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("words".into())));
        visual.simulate_keystrokes("cmd-v");
        assert_eq!(actions.borrow().len(), 1, "text is the field's to paste");
    }

    #[test]
    fn an_image_pasted_into_a_queued_prompt_s_editor_is_not_attached_to_the_draft() {
        let image =
            gpui::Image::from_bytes(gpui::ImageFormat::Png, b"\x89PNG\r\n\x1a\nrest".to_vec());
        let mut test = TestAppContext::single();
        test.update(crate::commands::install_key_bindings);
        let actions = Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = actions.clone();
        let (field, editor) = test.update(|cx| {
            (
                cx.new(TextField::new),
                cx.new(|cx| TextField::new(cx).multiline(4.0)),
            )
        });
        let chrome = ComposerChrome {
            busy: true,
            attachments_enabled: true,
            queued: vec!["queued".into()],
            editing_queued: Some((0, editor.clone())),
            ..live()
        }
        .on_action(move |action, _, _| recorded.borrow_mut().push(action));
        let window = test.add_window(move |_, _| Harness::new(field.clone(), chrome, ""));
        test.refresh().expect("painted");
        let focused = editor.clone();
        window
            .update(&mut test, |_, window, cx| {
                focused.read(cx).focus_handle().focus(window)
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image)));
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_keystrokes("cmd-v");
        assert!(
            !actions
                .borrow()
                .iter()
                .any(|action| matches!(action, ComposerAction::PasteImage(_))),
            "the image went to the draft the reader was not editing"
        );
    }

    #[test]
    fn pasting_an_image_attaches_it_with_the_apps_own_key_bindings() {
        // The app binds ⌘V to the field's Paste. GPUI runs a bound action
        // before any key-down listener and stops, so the key-down check never
        // saw ⌘V in the app, and an image was never attached. The test above
        // binds nothing, which is why it passed all along.
        let image =
            gpui::Image::from_bytes(gpui::ImageFormat::Png, b"\x89PNG\r\n\x1a\nrest".to_vec());
        let (mut test, window, field, actions) = recording(
            ComposerChrome {
                attachments_enabled: true,
                ..live()
            },
            "",
        );
        test.update(crate::commands::install_key_bindings);
        focus(&mut test, &window, &field);
        test.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image)));
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_keystrokes("cmd-v");
        assert_eq!(*actions.borrow(), vec![ComposerAction::PasteImage(image)]);

        test.update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("words".into())));
        visual.simulate_keystrokes("cmd-v");
        assert_eq!(actions.borrow().len(), 1, "text is the field's to paste");
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
            context_usage: Some(ContextUsage {
                used: Some(200_000),
                limit: Some(200_000),
            }),
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
            // The card's own padding and border, and nothing else: any larger
            // gap means the control is not on this row's trailing edge at all.
            let expected = Spacing::MD + BorderWidth::REGULAR;
            assert!(
                (inset - expected).abs() < 1.0,
                "the primary control sits {inset}pt inside the card, not {expected}pt"
            );
        });
    }
}
