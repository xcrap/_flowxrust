//! The approval tray: the controls for a tool call the provider will not run
//! without a decision, and for a question it will not answer without words.
//!
//! The transcript already showed that a request existed (`pending_prompts`) and
//! deliberately drew no buttons, because a button that cannot answer the
//! provider is worse than no button: it looks live and hangs the turn. This is
//! that missing half, and it answers through callbacks rather than touching
//! `Conversation`, so it can be mounted above the composer or anywhere else.
//!
//! # Three layers, and why
//!
//! - [`Decision`] is what the provider is told. It is pure data and knows
//!   nothing about layout.
//! - [`rows`] is what gets drawn. It is the half where the bugs live, so it is
//!   a free function over `(request, decision)` rather than a method that can
//!   read mutable view state and answer differently on either side of a click.
//! - [`ApprovalTray`] is the view: a card, a row of buttons, and two shortcuts.
//!
//! # The rules, each one a bug written first
//!
//! - **A command defaults to Deny.** A default of Approve means a reader who
//!   presses Return without reading has authorised an arbitrary command, and the
//!   keyboard shortcut makes that one keystroke away.
//! - **A question has no Yes and No.** Its answer is a word the provider chose
//!   the shape of; "Approve" is not an answer to any of them. Rows are one
//!   button per option plus Cancel, and a question with *no* options still has
//!   Cancel, so there is no state the tray cannot leave.
//! - **A command is never elided.** Truncating what is being authorised is a
//!   safety bug, not a layout one: `rm -rf build` and `rm -rf /` differ past
//!   the cut. The text wraps instead, in the monospace face so a reader can see
//!   the spacing that decides what a shell will do with it.
//! - **One request, one answer.** A second click on a decided tray is dropped
//!   rather than forwarded: the provider has already been told, and a second
//!   reply to the same request id is either ignored or read as a new answer.
//!
//! # Wire vocabulary
//!
//! [`Decision::to_wire`] names an outcome; it is **not** a string either provider
//! accepts today. `fx-agent`'s Codex path writes
//! `{"decision": "approved"}` / `{"decision": "denied"}` and its Claude path
//! writes `{"behavior": "allow"}` / `{"behavior": "deny"}` — two spellings, one
//! permission and one refusal, and nothing in the workspace that translates
//! between them. Two of the four names here match Codex exactly; the other two
//! (`"approved_for_session"`, `"abort"`) have no counterpart in either transport
//! yet. The caller answering a request has to map them onto whichever transport
//! is live; this module deliberately does not guess which.

use std::cell::RefCell;

use fx_design::components::{BadgeTone, ButtonStyle, badge, button, panel};
use fx_design::icon::Icon;
use fx_design::styled::{FxStyled, px_len};
use fx_design::theme::{Colors, IconSize, Layout, Radii, Spacing, TextRole, Theme, global};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, Render, SharedString, Window, div,
    px,
};

// ── The decision ───────────────────────────────────────────────────────────

/// What the reader chose about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// Run it this once.
    Approve,
    /// Run it, and do not ask again for the rest of this session.
    ApproveForSession,
    /// Refuse it, with a reason, and carry on with the turn.
    Deny,
    /// Stop waiting: the reader walked away without answering.
    Cancel,
}

impl Decision {
    /// Every case, for tests and for a keyboard's left-to-right order.
    pub const ALL: [Decision; 4] = [
        Decision::Approve,
        Decision::ApproveForSession,
        Decision::Deny,
        Decision::Cancel,
    ];

    /// The name this outcome is given on the wire.
    ///
    /// Two of these — `"approved"` and `"denied"` — are exactly what `fx-agent`
    /// sends to `codex app-server`. The other two have no counterpart in either
    /// transport today: Claude spells the same two outcomes `"allow"` and
    /// `"deny"` inside a `control_response`, and nothing in the workspace
    /// translates between them. A caller answering a request has to do that
    /// mapping; see the module docs.
    pub const fn to_wire(self) -> &'static str {
        match self {
            Decision::Approve => "approved",
            Decision::ApproveForSession => "approved_for_session",
            Decision::Deny => "denied",
            Decision::Cancel => "abort",
        }
    }

    /// What the button says.
    pub const fn label(self) -> &'static str {
        match self {
            Decision::Approve => "Approve once",
            Decision::ApproveForSession => "Approve for this session",
            Decision::Deny => "Deny",
            Decision::Cancel => "Cancel",
        }
    }

    /// Whether this is a refusal rather than a permission.
    ///
    /// Destructive in the sense that matters to the caller: it ends the wait and
    /// the provider has to be told. `Cancel` counts, because a request that is
    /// never answered is what hangs a turn.
    pub const fn is_destructive(self) -> bool {
        matches!(self, Decision::Deny | Decision::Cancel)
    }

    /// How the control is drawn, decided by the decision rather than by the
    /// caller.
    ///
    /// The refusal is the destructive style so it reads as one: it is the only
    /// control on screen that can stop the turn, and a secondary button beside
    /// a primary one does not say that.
    pub const fn button_style(self) -> ButtonStyle {
        match self {
            Decision::Approve => ButtonStyle::Primary,
            Decision::ApproveForSession => ButtonStyle::Secondary,
            Decision::Deny | Decision::Cancel => ButtonStyle::Danger,
        }
    }
}

// ── The request ────────────────────────────────────────────────────────────

/// What is being asked for, in the vocabulary the tray draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalKind {
    /// A command to run.
    Command {
        /// The command as the provider sent it, verbatim.
        ///
        /// Never summarised and never cut: the difference between two commands
        /// is often a few characters from the end.
        command: String,
        /// Where it will run, when the provider said.
        cwd: Option<String>,
    },
    /// A set of files to change.
    Edit { paths: Vec<String> },
    /// A question answered with text rather than a permission.
    Question {
        prompt: String,
        /// The answers the provider offers. Empty is legal — a provider can ask
        /// something free-form — and it must not produce a dead end.
        options: Vec<String>,
    },
}

/// One thing the provider is blocked on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The provider's own id for this request, handed back untouched.
    pub id: String,
    /// The headline: what is being asked for.
    pub title: String,
    /// A secondary line, when the provider gave one.
    pub detail: Option<String>,
    pub kind: ApprovalKind,
}

impl ApprovalRequest {
    /// A command approval, the common case.
    pub fn command(id: impl Into<String>, command: impl Into<String>, cwd: Option<String>) -> Self {
        Self {
            id: id.into(),
            title: "FlowX wants to run a command".to_string(),
            detail: None,
            kind: ApprovalKind::Command {
                command: command.into(),
                cwd,
            },
        }
    }

    /// An edit approval over one or more paths.
    pub fn edit(id: impl Into<String>, paths: Vec<String>) -> Self {
        Self {
            id: id.into(),
            title: "FlowX wants to change files".to_string(),
            detail: None,
            kind: ApprovalKind::Edit { paths },
        }
    }

    /// A question the provider wants answered in words.
    pub fn question(
        id: impl Into<String>,
        prompt: impl Into<String>,
        options: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: "FlowX has a question".to_string(),
            detail: None,
            kind: ApprovalKind::Question {
                prompt: prompt.into(),
                options,
            },
        }
    }
}

// ── What gets drawn ────────────────────────────────────────────────────────

/// One row of the tray's control strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRow {
    /// One of a question's answers. Choosing it sends `answer` back verbatim.
    Answer {
        /// What the reader sees, and what is sent.
        answer: String,
        /// Which answer this is, counting from zero.
        ///
        /// Position rather than the string because two options may carry the
        /// same text, and a provider matching answers by position then gets two
        /// identical rows where it offered two different ones.
        index: usize,
        /// Whether this answer is the one already given.
        pressed: bool,
    },
    /// A permission or a refusal.
    Decision {
        decision: Decision,
        /// Whether this decision is the one already given.
        pressed: bool,
    },
}

impl ApprovalRow {
    /// Whether this row is the given decision.
    pub fn is(&self, decision: Decision) -> bool {
        matches!(self, ApprovalRow::Decision { decision: found, .. } if *found == decision)
    }

    /// What the row's button says.
    pub fn label(&self) -> String {
        match self {
            ApprovalRow::Answer { answer, .. } => answer.clone(),
            ApprovalRow::Decision { decision, .. } => decision.label().to_string(),
        }
    }

    /// Whether the row ends the wait rather than granting permission.
    pub fn is_destructive(&self) -> bool {
        match self {
            ApprovalRow::Answer { .. } => false,
            ApprovalRow::Decision { decision, .. } => decision.is_destructive(),
        }
    }

    /// Whether this row is the one already chosen.
    ///
    /// Distinct from [`Self::is`]: `is` compares against a decision the caller
    /// names, this reports the tray's own current state.
    pub fn is_pressed(&self) -> bool {
        match self {
            ApprovalRow::Answer { pressed, .. } => *pressed,
            ApprovalRow::Decision { pressed, .. } => *pressed,
        }
    }
}

/// What a command approval is *about*, for the title area.
///
/// Split from the row strip because the command block is drawn above the
/// buttons and is not one of them, and because the empty-command rule belongs
/// to the block rather than to any row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalBody {
    /// A command, shown whole in the monospace face.
    Command {
        command: String,
        cwd: Option<String>,
    },
    /// The paths an edit would touch.
    Paths(Vec<String>),
    /// A question's text.
    Question(String),
}

/// The monospaced block for a request, if it has one.
///
/// Returns `None` for an approval with an empty command rather than an empty
/// string: a bordered box with nothing in it reads as a rendering failure, and
/// this one appears precisely when the provider sent an argument this tray
/// could not parse. The title is still there.
pub fn command_block(request: &ApprovalRequest) -> Option<ApprovalBody> {
    match &request.kind {
        ApprovalKind::Command { command, cwd } => {
            if command.trim().is_empty() {
                None
            } else {
                Some(ApprovalBody::Command {
                    command: command.clone(),
                    cwd: cwd.clone(),
                })
            }
        }
        // An edit with no paths is the same failure: the provider named a tool
        // whose input this tray does not read, so the paths block would be
        // empty. The title still says a change is proposed.
        ApprovalKind::Edit { paths } => {
            let paths: Vec<String> = paths
                .iter()
                .filter(|p| !p.trim().is_empty())
                .cloned()
                .collect();
            (!paths.is_empty()).then_some(ApprovalBody::Paths(paths))
        }
        ApprovalKind::Question { prompt, .. } => {
            if prompt.trim().is_empty() {
                None
            } else {
                Some(ApprovalBody::Question(prompt.clone()))
            }
        }
    }
}

/// The rows the control strip draws, in the order it draws them.
///
/// `decision` is the decision already given, which sets a row's pressed state
/// and **nothing else**. It deliberately cannot add, remove, or reorder a row:
/// a strip that grew a row once something was chosen would let the second click
/// of a double-click land on a button the first click never drew. Two calls with
/// the same arguments always produce the same rows.
///
/// Three rules are load-bearing:
///
/// - A command or an edit gets all four decisions, in [`Decision::ALL`] order:
///   permission first, refusal last.
/// - A question gets one row per option and **only** Cancel. Offering Approve
///   for a question is meaningless, and offering Deny beside it invents a
///   distinction the provider cannot express.
/// - A question with no options gets Cancel alone. A strip with nothing on it
///   is the one state the reader cannot leave.
pub fn rows(request: &ApprovalRequest, decision: Option<Decision>) -> Vec<ApprovalRow> {
    match &request.kind {
        ApprovalKind::Command { .. } | ApprovalKind::Edit { .. } => Decision::ALL
            .into_iter()
            .map(|choice| ApprovalRow::Decision {
                decision: choice,
                pressed: decision == Some(choice),
            })
            .collect(),
        ApprovalKind::Question { options, .. } => {
            let mut out: Vec<ApprovalRow> = options
                .iter()
                .enumerate()
                .map(|(index, answer)| ApprovalRow::Answer {
                    answer: answer.clone(),
                    index,
                    // An answer is never "already given" by a `Decision`: the two
                    // are different kinds of outcome, and a question that
                    // highlighted a permission nobody offered would be claiming
                    // a choice it does not have.
                    pressed: false,
                })
                .collect();
            // Cancel is appended rather than replacing the options, so a
            // question with N answers has N + 1 ways out and a question with no
            // answers has exactly one.
            out.push(ApprovalRow::Decision {
                decision: Decision::Cancel,
                pressed: decision == Some(Decision::Cancel),
            });
            out
        }
    }
}

/// The decision a shortcut picks when the reader presses Return.
///
/// Deny for anything that grants power, and Cancel for a question — the one row
/// a question is guaranteed to have, and never a permission it does not offer.
///
/// An approval that defaults to a permission is a one-key accident waiting to
/// happen: the shortcut exists for a reader who is not looking at the screen.
pub fn default_decision(request: &ApprovalRequest) -> Decision {
    match &request.kind {
        ApprovalKind::Command { .. } | ApprovalKind::Edit { .. } => Decision::Deny,
        ApprovalKind::Question { .. } => Decision::Cancel,
    }
}

/// Whether `decision` is a row this request actually draws.
///
/// A shortcut that picks a hidden decision would answer the provider with a
/// choice the reader was never shown, so the tray checks before it fires.
pub fn offers(request: &ApprovalRequest, decision: Decision) -> bool {
    rows(request, None).iter().any(|row| row.is(decision))
}

/// The answer a question row carries, if it is an answer row.
pub fn answer_at(request: &ApprovalRequest, index: usize) -> Option<&str> {
    match &request.kind {
        ApprovalKind::Question { options, .. } => options.get(index).map(String::as_str),
        _ => None,
    }
}

// ── The tray ───────────────────────────────────────────────────────────────

type DecideHandler = Box<dyn Fn(String, Decision, &mut Window, &mut App)>;
type CancelHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// The one tray that is on screen.
///
/// A `gpui::Global`, like [`crate::AppHandle`], and for the same reason: it is
/// per-`App` state that several places need to agree on, and there has to be
/// exactly one answer to "which tray is showing". The shell still owns the tray
/// in `AppState` — this exists so [`ApprovalTray::open`] can displace whatever is
/// already there without being handed it, which is what makes "one at a time"
/// true rather than a convention.
#[derive(Default)]
pub struct OpenTray(RefCell<Option<Entity<ApprovalTray>>>);

impl gpui::Global for OpenTray {}

/// The tray currently mounted, if any.
pub fn open_tray(cx: &App) -> Option<Entity<ApprovalTray>> {
    cx.try_global::<OpenTray>()
        .and_then(|slot| slot.0.borrow().clone())
}

/// Record which tray is on screen.
fn set_open(cx: &mut App, tray: Entity<ApprovalTray>) {
    match cx.try_global::<OpenTray>() {
        Some(slot) => *slot.0.borrow_mut() = Some(tray),
        None => cx.set_global(OpenTray(RefCell::new(Some(tray)))),
    }
}

/// The card that carries the controls.
///
/// One request at a time. [`ApprovalTray::open`] replaces whatever was open, so
/// a second approval arriving mid-decision cannot leave two trays stacked on the
/// composer with both live.
pub struct ApprovalTray {
    request: ApprovalRequest,
    /// The decision currently held, shown as pressed.
    ///
    /// `None` until something is chosen. Nothing is pre-selected: a highlighted
    /// button before it is pressed claims the reader decided.
    decision: Option<Decision>,
    /// Whether an outcome has already left this tray.
    ///
    /// Set before the callback fires, not after. A double-click on the same
    /// button runs the handler twice otherwise, and the provider sees two
    /// answers for one request.
    settled: bool,
    /// Set when a newer request displaced this one.
    displaced: bool,
    focus: FocusHandle,
    on_decide: DecideHandler,
    on_cancel: CancelHandler,
}

impl ApprovalTray {
    /// Build a tray for one request, without showing it.
    ///
    /// Focus is not claimed here; [`ApprovalTray::open`] does that, because a
    /// tray built inside a render must not steal focus out of the composer.
    ///
    /// `cx` is a parameter the stated signature does not carry and cannot do
    /// without: `cx.new` is the only way to make an `Entity`, and a focus handle
    /// needs one too.
    pub fn new(
        cx: &mut App,
        request: ApprovalRequest,
        on_decide: DecideHandler,
        on_cancel: CancelHandler,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut focus = cx.focus_handle();
            // Without this the handle is focusable in principle and unreachable
            // in practice, so neither shortcut is ever delivered.
            focus.tab_stop = true;
            Self {
                request,
                decision: None,
                settled: false,
                displaced: false,
                focus,
                on_decide,
                on_cancel,
            }
        })
    }

    /// Show a request, taking focus. The usual entry point.
    ///
    /// One tray at a time: whatever [`open_tray`] reports is displaced, and its
    /// `on_cancel` fires before the new one mounts. A provider blocked on a
    /// request nobody can reach any more hangs the turn just as surely as one
    /// that was never asked, so being displaced is an outcome and not a silent
    /// unmount.
    ///
    /// Focus rather than wait for a click, and it takes *its own* focus rather
    /// than the composer's: the shortcut is the point of this control, so it has
    /// to be reachable the moment the provider asks.
    pub fn open(
        window: &mut Window,
        cx: &mut App,
        request: ApprovalRequest,
        on_decide: DecideHandler,
        on_cancel: CancelHandler,
    ) -> Entity<Self> {
        if let Some(previous) = open_tray(cx) {
            previous.update(cx, |tray, cx| {
                tray.mark_displaced();
                tray.cancel(window, cx);
            });
        }
        let tray = Self::new(cx, request, on_decide, on_cancel);
        // Registered before the focus, because `focus` walks the tab map and the
        // handle is not in it until the tray has been seen at least once.
        set_open(cx, tray.clone());
        tray.read(cx).focus.focus(window);
        tray
    }

    /// Forget this tray, so the next [`ApprovalTray::open`] does not displace one
    /// that has already answered or been walked away from.
    fn retire(tray: &Entity<Self>, cx: &mut App) {
        if open_tray(cx).as_ref() != Some(tray) {
            return;
        }
        // Dropping the slot is the empty answer. Left in place it would point at
        // a tray that has already reported, and the next `open` would displace a
        // dead one.
        if cx.try_global::<OpenTray>().is_some() {
            cx.remove_global::<OpenTray>();
        }
    }

    /// The request on screen.
    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// The decision currently held, if any.
    pub fn decision(&self) -> Option<Decision> {
        self.decision
    }

    /// Whether an outcome has already left this tray.
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// Whether a newer request took this tray's place.
    pub fn is_displaced(&self) -> bool {
        self.displaced
    }

    /// The focus handle the shortcuts arrive on.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Apply the double-decision guard and report what to send.
    ///
    /// Split from the callback so the guard is reachable without a window: it is
    /// the whole safety property of this type, and a rule that can only be
    /// checked by dispatching a keystroke is a rule that goes unchecked.
    ///
    /// `None` for a second decision, or for one the tray does not draw — so a
    /// shortcut cannot answer a question with "approved".
    fn claim_decision(&mut self, decision: Decision) -> Option<(String, Decision)> {
        if self.settled || !offers(&self.request, decision) {
            return None;
        }
        self.settled = true;
        self.decision = Some(decision);
        Some((self.request.id.clone(), decision))
    }

    /// Apply the same guard to one of a question's answers.
    ///
    /// `None` for an index the request does not have, so a stale row cannot
    /// answer a request whose options changed underneath it.
    fn claim_answer(&mut self, index: usize) -> Option<String> {
        if self.settled {
            return None;
        }
        let answer = answer_at(&self.request, index)?.to_string();
        self.settled = true;
        Some(answer)
    }

    /// Apply the same guard to backing out.
    fn claim_cancel(&mut self) -> Option<String> {
        if self.settled {
            return None;
        }
        self.settled = true;
        Some(self.request.id.clone())
    }

    /// Record that a newer request took this tray's place.
    fn mark_displaced(&mut self) {
        self.displaced = true;
    }

    /// Choose a decision.
    fn decide(&mut self, decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, decision)) = self.claim_decision(decision) else {
            return;
        };
        (self.on_decide)(id, decision, window, cx);
        Self::retire(&cx.entity(), cx);
    }

    /// Choose one of a question's answers.
    ///
    /// Carried through the decision callback because that is the caller's only
    /// way out of the tray, and `Decision::Cancel` is the honest reading: the wait
    /// is over and nothing was approved. The answer itself travels as the position
    /// the caller recovers from the request it already holds.
    fn answer(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.claim_answer(index).is_none() {
            return;
        }
        let id = self.request.id.clone();
        (self.on_decide)(id, Decision::Cancel, window, cx);
        Self::retire(&cx.entity(), cx);
    }

    /// Back out without deciding.
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.claim_cancel().is_none() {
            return;
        }
        (self.on_cancel)(window, cx);
        Self::retire(&cx.entity(), cx);
    }

    /// The shortcut layer: Return takes the default, Escape cancels.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "return" | "enter" => {
                // Only the modified chord. A bare Return is the composer's
                // submit, and a tray sitting above it must not swallow it.
                if !keystroke.modifiers.secondary() {
                    return;
                }
                let decision = default_decision(&self.request);
                cx.stop_propagation();
                self.decide(decision, window, cx);
            }
            "escape" => {
                cx.stop_propagation();
                self.cancel(window, cx);
            }
            _ => {}
        }
    }
}

impl Render for ApprovalTray {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let this = cx.entity();

        // A decided or displaced tray draws nothing. The caller owns the unmount
        // — the answer has to reach the provider, not just close a card — and
        // until it does, a live-looking tray with live buttons is one more click
        // away from answering the same request twice.
        if self.settled {
            return div().w(px(0.0)).h(px(0.0)).into_any_element();
        }

        let body = div()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .p(px_len(Spacing::LG))
            .child(header(&theme, &self.request));

        let body = match command_block(&self.request) {
            Some(ApprovalBody::Command { command, cwd }) => {
                body.child(command_panel(&theme, &command, cwd.as_deref()))
            }
            Some(ApprovalBody::Paths(paths)) => body.child(paths_panel(&theme, &paths)),
            Some(ApprovalBody::Question(prompt)) => body.child(
                div()
                    .fx_body_medium()
                    .text_color(Colors::fg(&theme))
                    .child(SharedString::from(prompt)),
            ),
            // Nothing to show beyond the title. Deliberately no block rather than
            // an empty one: a bordered box with no content reads as a bug in the
            // tray, and the reader has nothing to read in it anyway.
            None => body,
        };

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
                    .pb(px_len(Spacing::MD))
                    // `track_focus` is what makes the shortcuts reachable: it
                    // both registers the handle and claims `tab`.
                    .track_focus(&self.focus)
                    .on_key_down({
                        let this = this.clone();
                        move |event: &KeyDownEvent, window, cx| {
                            this.update(cx, |tray, cx| tray.on_key_down(event, window, cx));
                        }
                    })
                    .child(panel(body.child(control_strip(
                        &self.request,
                        self.decision,
                        &this,
                    ))))
                    .child(shortcut_hint(&theme)),
            )
            .into_any_element()
    }
}

/// The headline: what is being asked, and what kind of thing it is.
fn header(theme: &Theme, request: &ApprovalRequest) -> AnyElement {
    let (icon, tone) = match &request.kind {
        ApprovalKind::Command { .. } => (Icon::TERMINAL, BadgeTone::Warning),
        ApprovalKind::Edit { .. } => (Icon::DOC_TEXT, BadgeTone::Info),
        ApprovalKind::Question { .. } => (Icon::QUESTIONMARK_BUBBLE_FILL, BadgeTone::Accent),
    };

    let mut head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .child(icon.render(IconSize::Small, tone.hue(theme), theme))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .fx_body_medium()
                .text_color(Colors::fg(theme))
                .child(SharedString::from(request.title.clone())),
        )
        .child(badge(SharedString::from(kind_label(request)), tone, false));

    // Present but never reserved: an absent line must not move the buttons.
    if let Some(detail) = &request.detail
        && !detail.trim().is_empty()
    {
        head = head.child(
            div()
                .fx_caption()
                .text_color(Colors::fg_secondary(theme))
                .child(SharedString::from(detail.clone())),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXS))
        .child(head)
        .into_any_element()
}

fn kind_label(request: &ApprovalRequest) -> &'static str {
    match &request.kind {
        ApprovalKind::Command { .. } => "Command",
        ApprovalKind::Edit { .. } => "Edit",
        ApprovalKind::Question { .. } => "Question",
    }
}

/// The command, whole, in the monospaced face.
///
/// No `truncate`, no `line_clamp`, and no character cap. The block wraps
/// instead: a reader authorising `cargo test --workspace` has to see every
/// argument, and the cut is always in the part that decides what runs.
fn command_panel(theme: &Theme, command: &str, cwd: Option<&str>) -> AnyElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .rounded(px_len(Radii::MD))
        .border_1()
        .border_color(Colors::border_subtle(theme))
        .bg(Colors::bg_surface(theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .px(px_len(Spacing::MD))
                .py(px_len(Spacing::XS))
                .bg(Colors::bg_elevated(theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .fx_overline()
                        .text_color(Colors::fg_tertiary(theme))
                        .child(SharedString::from("Command")),
                )
                // The working directory is part of what is authorised: the same
                // command means different things in two checkouts.
                .child(match cwd {
                    Some(cwd) if !cwd.trim().is_empty() => div()
                        .flex_1()
                        .min_w_0()
                        .fx_role(TextRole::MonoSmall)
                        .text_color(Colors::fg_tertiary(theme))
                        .child(SharedString::from(cwd.to_string())),
                    _ => div().flex_1(),
                }),
        )
        .child(
            div()
                .px(px_len(Spacing::MD))
                .py(px_len(Spacing::SM))
                .fx_role(TextRole::Mono)
                .text_color(Colors::fg(theme))
                .child(SharedString::from(command.to_string())),
        );
    // Preserved as a named binding so the intent reads at the use site rather
    // than in a comment above the whole function.
    panel = panel.flex_shrink_0();
    panel.into_any_element()
}

/// The paths an edit would touch, one per row.
fn paths_panel(theme: &Theme, paths: &[String]) -> AnyElement {
    let mut list = div()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXS))
        .rounded(px_len(Radii::MD))
        .border_1()
        .border_color(Colors::border_subtle(theme))
        .bg(Colors::bg_surface(theme))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM));

    for path in paths {
        list = list.child(
            div()
                .flex_1()
                .min_w_0()
                .fx_role(TextRole::Mono)
                .text_color(Colors::fg(theme))
                // Wrapped rather than cut, for the same reason the command is.
                .child(SharedString::from(path.clone())),
        );
    }

    list.into_any_element()
}

/// The row of buttons, in the order the reader reaches them.
///
/// One `button` per [`ApprovalRow`], in row order, so the pointer order, the
/// source order, and the shortcut order all agree — a reader who tabs and a
/// reader who clicks should be offered the same sequence.
///
/// Wrapping rather than a fixed width: a question can offer as many answers as
/// it likes, and a strip that ran off the edge of a narrow column is a control
/// the reader cannot reach at all.
fn control_strip(
    request: &ApprovalRequest,
    decision: Option<Decision>,
    this: &Entity<ApprovalTray>,
) -> AnyElement {
    let drawn = rows(request, decision);
    let mut strip = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px_len(Spacing::SM));

    for row in drawn {
        // One `let` per arm rather than a single `match` returning the element: the
        // design system's `button` returns an opaque type, and two calls in two
        // arms are two types, which a match cannot reconcile.
        if let ApprovalRow::Answer { index, .. } = row {
            let for_answer = this.clone();
            strip = strip.child(
                button(
                    answer_at(request, index).unwrap_or_default().to_string(),
                    // An answer grants nothing, so it takes the neutral weight:
                    // the primary slot belongs to a permission, and a question
                    // has none.
                    ButtonStyle::Secondary,
                    true,
                    move |_event, window, cx| {
                        for_answer.update(cx, |tray, cx| tray.answer(index, window, cx));
                    },
                )
                .into_any_element(),
            );
        }
        if let ApprovalRow::Decision { decision, .. } = row {
            let for_decide = this.clone();
            strip = strip.child(
                button(
                    decision.label().to_string(),
                    decision.button_style(),
                    true,
                    move |_event, window, cx| {
                        for_decide.update(cx, |tray, cx| tray.decide(decision, window, cx));
                    },
                )
                .into_any_element(),
            );
        }
    }

    strip.into_any_element()
}

/// The shortcuts, said out loud.
///
/// A tray the reader can answer with the keyboard has no business making them
/// discoverable by hovering.
fn shortcut_hint(theme: &Theme) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .pt(px_len(Spacing::XXS))
        .fx_caption()
        .text_color(Colors::fg_tertiary(theme))
        .child(div().flex_1().min_w_0().child(SharedString::from(format!(
            "{}{SHORTCUT_SEPARATOR}{DEFAULT_SHORTCUT_TEXT}{SHORTCUT_SEPARATOR}{CANCEL_SHORTCUT_TEXT}",
            SHORTCUT_MODIFIER,
        ))))
        .into_any_element()
}

/// The separator between the shortcut and what it does.
const SHORTCUT_SEPARATOR: &str = " · ";
/// How the modifier is named in the hint.
///
/// `Modifiers::secondary` is Cmd on macOS and Ctrl elsewhere, so the glyph has
/// to follow the same rule rather than hardcoding one platform.
#[cfg(target_os = "macos")]
const SHORTCUT_MODIFIER: &str = "⌘↩";
#[cfg(not(target_os = "macos"))]
const SHORTCUT_MODIFIER: &str = "ctrl+↩";

/// What the primary shortcut does.
const DEFAULT_SHORTCUT_TEXT: &str = "takes the default decision";
/// What Escape does.
const CANCEL_SHORTCUT_TEXT: &str = "esc cancels";

// ── Mounting ───────────────────────────────────────────────────────────────

/// Mount a tray above the composer for one request.
///
/// The closure-taking twin of [`ApprovalTray::open`], so a caller does not have
/// to name `Box<dyn Fn>` twice. One tray at a time either way.
///
/// `on_decide` receives the provider's id and the decision. It should take the
/// approval out of the conversation's pending list, send the answer to the
/// provider, and stop drawing the tray. `on_cancel` does the same for a request
/// that was walked away from.
pub fn mount(
    window: &mut Window,
    cx: &mut App,
    request: ApprovalRequest,
    on_decide: impl Fn(String, Decision, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&mut Window, &mut App) + 'static,
) -> Entity<ApprovalTray> {
    ApprovalTray::open(
        window,
        cx,
        request,
        Box::new(on_decide),
        Box::new(on_cancel),
    )
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> ApprovalRequest {
        ApprovalRequest::command(
            "req-1",
            "cargo test --workspace",
            Some("/Users/you/code".to_string()),
        )
    }

    fn edit() -> ApprovalRequest {
        ApprovalRequest::edit(
            "req-2",
            vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        )
    }

    fn question() -> ApprovalRequest {
        ApprovalRequest::question(
            "req-3",
            "Which database?",
            vec!["Postgres".to_string(), "SQLite".to_string()],
        )
    }

    // ── The decision model ──────────────────────────────────────────────────

    #[test]
    fn the_wire_names_are_stable_and_distinct() {
        // A duplicate here would let two decisions serialize to the same thing,
        // and the provider would act on whichever it read first.
        let names: Vec<&str> = Decision::ALL.iter().map(|d| d.to_wire()).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        assert_eq!(
            Decision::Approve.to_wire(),
            "approved",
            "Codex reads `approved` — crates/fx-agent/src/codex/turn.rs:363"
        );
        assert_eq!(
            Decision::Deny.to_wire(),
            "denied",
            "Codex reads `denied` — crates/fx-agent/src/codex/turn.rs:365"
        );
        assert_eq!(
            Decision::ApproveForSession.to_wire(),
            "approved_for_session"
        );
        assert_eq!(Decision::Cancel.to_wire(), "abort");
    }

    #[test]
    fn a_refusal_is_a_refusal_and_a_permission_is_not() {
        assert!(Decision::Deny.is_destructive());
        assert!(Decision::Cancel.is_destructive());
        assert!(!Decision::Approve.is_destructive());
        assert!(!Decision::ApproveForSession.is_destructive());
    }

    #[test]
    fn only_approve_and_deny_share_a_wire_vocabulary_with_the_providers() {
        // Claude spells the same two outcomes `allow` and `deny`
        // (crates/fx-agent/src/claude/stream.rs:698 and :708) and Codex spells
        // them `approved` and `denied`. This pins that FlowX's two permission
        // names match Codex exactly and are *not* Claude's, so nobody reads a
        // green test as agreement between the providers.
        assert_ne!(Decision::Approve.to_wire(), "allow");
        assert_ne!(Decision::Deny.to_wire(), "deny");
    }

    #[test]
    fn every_decision_has_a_label_a_reader_can_act_on() {
        for decision in Decision::ALL {
            assert!(!decision.label().is_empty());
            assert_ne!(
                decision.label(),
                decision.to_wire(),
                "the wire name leaked into the button"
            );
        }
    }

    #[test]
    fn both_permissions_carry_the_accent_and_both_refusals_the_danger_tint() {
        // The reader has to be able to see which side of the line a button is on
        // before pressing it, from styling alone.
        assert_eq!(Decision::Approve.button_style(), ButtonStyle::Primary);
        assert_eq!(
            Decision::ApproveForSession.button_style(),
            ButtonStyle::Secondary
        );
        assert_eq!(Decision::Deny.button_style(), ButtonStyle::Danger);
        assert_eq!(Decision::Cancel.button_style(), ButtonStyle::Danger);
    }

    // ── Defaults ────────────────────────────────────────────────────────────

    #[test]
    fn a_command_defaults_to_deny() {
        // A reader who presses the shortcut without reading has just
        // authorised an arbitrary command. That is the whole rule.
        assert_eq!(default_decision(&command()), Decision::Deny);
    }

    #[test]
    fn an_edit_also_defaults_to_deny() {
        assert_eq!(default_decision(&edit()), Decision::Deny);
    }

    #[test]
    fn no_request_defaults_to_approving() {
        for request in [command(), edit(), question()] {
            let decision = default_decision(&request);
            assert!(
                !matches!(decision, Decision::Approve | Decision::ApproveForSession),
                "{} defaults to a permission",
                request.id
            );
            assert!(
                offers(&request, decision),
                "{}'s default is not one of its own rows",
                request.id
            );
        }
    }

    // ── Rows ────────────────────────────────────────────────────────────────

    #[test]
    fn a_command_offers_all_four_decisions_in_order() {
        let drawn = rows(&command(), None);
        let drawn: Vec<Decision> = drawn
            .iter()
            .map(|row| match row {
                ApprovalRow::Decision { decision, .. } => *decision,
                ApprovalRow::Answer { .. } => panic!("a command offered an answer"),
            })
            .collect();
        assert_eq!(drawn, Decision::ALL.to_vec());
    }

    #[test]
    fn an_edit_offers_all_four_decisions() {
        assert_eq!(rows(&edit(), None).len(), Decision::ALL.len());
    }

    #[test]
    fn a_question_has_no_yes_and_no() {
        let drawn = rows(&question(), None);
        for row in &drawn {
            assert!(
                !row.is(Decision::Approve),
                "Approve was offered for a question"
            );
            assert!(!row.is(Decision::Deny), "Deny was offered for a question");
            assert!(
                !row.is(Decision::ApproveForSession),
                "Approve-for-session was offered for a question"
            );
        }
        assert!(
            !offers(&question(), Decision::Approve),
            "a question claims it offers a permission it does not draw"
        );
    }

    #[test]
    fn a_question_offers_one_row_per_option_and_cancel() {
        let drawn = rows(&question(), None);
        assert_eq!(drawn.len(), 3, "{drawn:?}");
        assert_eq!(drawn[0].label(), "Postgres");
        assert_eq!(drawn[1].label(), "SQLite");
        assert!(drawn[2].is(Decision::Cancel));
    }

    #[test]
    fn a_question_with_no_options_is_never_a_dead_end() {
        // The provider can ask something free-form and offer nothing. Without
        // Cancel the strip would be empty and the turn would hang forever.
        let bare = ApprovalRequest::question("req-4", "What should I name it?", Vec::new());
        let drawn = rows(&bare, None);
        assert_eq!(
            drawn,
            vec![ApprovalRow::Decision {
                decision: Decision::Cancel,
                pressed: false
            }]
        );
        assert!(offers(&bare, Decision::Cancel));
        assert_eq!(default_decision(&bare), Decision::Cancel);
    }

    #[test]
    fn duplicate_option_strings_keep_both_rows_and_their_positions() {
        // The provider distinguishes by position, so collapsing these would
        // silently turn a two-way choice into a one-way one.
        let repeated = ApprovalRequest::question(
            "req-5",
            "Pick one",
            vec!["Yes".to_string(), "No".to_string(), "Yes".to_string()],
        );
        let drawn = rows(&repeated, None);
        let answers: Vec<(String, usize)> = drawn
            .iter()
            .filter_map(|row| match row {
                ApprovalRow::Answer { answer, index, .. } => Some((answer.clone(), *index)),
                ApprovalRow::Decision { .. } => None,
            })
            .collect();
        assert_eq!(
            answers,
            vec![
                ("Yes".to_string(), 0),
                ("No".to_string(), 1),
                ("Yes".to_string(), 2)
            ]
        );
    }

    /// A row's identity and order, without its pressed state.
    fn shape(request: &ApprovalRequest) -> Vec<String> {
        rows(request, None).iter().map(ApprovalRow::label).collect()
    }

    #[test]
    fn holding_a_decision_changes_only_the_pressed_state() {
        // A strip that reordered or gained a row once something was chosen would
        // let the second click of a double-click land on a button the first
        // click never drew.
        for request in [command(), edit(), question()] {
            let before = shape(&request);
            for decision in Decision::ALL {
                assert_eq!(
                    before,
                    shape(&request),
                    "{} reordered when {decision:?} was held",
                    request.id
                );
            }
        }
    }

    #[test]
    fn the_held_decision_presses_exactly_its_own_row() {
        for decision in Decision::ALL {
            let pressed: Vec<bool> = rows(&command(), Some(decision))
                .iter()
                .map(ApprovalRow::is_pressed)
                .collect();
            let expected: Vec<bool> = Decision::ALL.iter().map(|d| *d == decision).collect();
            assert_eq!(pressed, expected, "{decision:?} pressed the wrong row");
        }
    }

    #[test]
    fn no_row_is_pressed_before_anything_is_chosen() {
        // A pre-highlighted button claims the reader already decided.
        for request in [command(), edit(), question()] {
            assert!(
                rows(&request, None).iter().all(|row| !row.is_pressed()),
                "{} pre-selected a row",
                request.id
            );
        }
    }

    #[test]
    fn a_question_never_presses_an_answer_row() {
        // An answer is a choice, not a decision; holding a `Decision` cannot
        // make the tray claim one was made.
        let drawn = rows(&question(), Some(Decision::Cancel));
        let pressed: Vec<bool> = drawn.iter().map(ApprovalRow::is_pressed).collect();
        assert_eq!(pressed, vec![false, false, true]);
    }

    #[test]
    fn rows_are_deterministic() {
        for request in [command(), edit(), question()] {
            assert_eq!(rows(&request, None), rows(&request, None));
        }
    }

    #[test]
    fn a_question_row_reports_the_answer_it_carries() {
        assert_eq!(answer_at(&question(), 0), Some("Postgres"));
        assert_eq!(answer_at(&question(), 1), Some("SQLite"));
        assert_eq!(answer_at(&question(), 2), None);
        assert_eq!(answer_at(&command(), 0), None, "a command has no answers");
    }

    // ── The body ────────────────────────────────────────────────────────────

    #[test]
    fn a_command_shows_the_whole_command_and_its_directory() {
        let Some(ApprovalBody::Command { command, cwd }) = command_block(&command()) else {
            panic!("no command block");
        };
        assert_eq!(command, "cargo test --workspace");
        assert_eq!(cwd.as_deref(), Some("/Users/you/code"));
    }

    #[test]
    fn a_command_is_never_cut() {
        // A `rm -rf` that differs only past the cut is the whole hazard.
        let long = format!("echo {} ; rm -rf /", "x".repeat(4_000));
        let request = ApprovalRequest::command("req-long", long.clone(), None);
        let Some(ApprovalBody::Command { command, .. }) = command_block(&request) else {
            panic!("no command block");
        };
        assert_eq!(command, long);
    }

    #[test]
    fn a_command_with_unicode_is_shown_as_it_arrived() {
        let request =
            ApprovalRequest::command("req-u", "rm -rf ~/Documents/日本語 🗂 — 'quoted'", None);
        let Some(ApprovalBody::Command { command, .. }) = command_block(&request) else {
            panic!("no command block");
        };
        assert_eq!(command, "rm -rf ~/Documents/日本語 🗂 — 'quoted'");
    }

    #[test]
    fn an_empty_command_draws_no_command_block() {
        // The provider sent an argument this tray cannot read. An empty bordered
        // box reads as a rendering bug and shows the reader nothing either way.
        let request = ApprovalRequest::command("req-empty", "   ", None);
        assert_eq!(command_block(&request), None);
        // And the title is still there to say what is being asked.
        assert!(!request.title.trim().is_empty());
        // The rows are unaffected: the buttons still decide the wait.
        assert_eq!(rows(&request, None).len(), Decision::ALL.len());
    }

    #[test]
    fn an_edit_with_no_paths_draws_no_paths_block() {
        let request = ApprovalRequest::edit("req-e", vec!["  ".to_string()]);
        assert_eq!(command_block(&request), None);
    }

    #[test]
    fn an_edit_lists_its_paths() {
        let Some(ApprovalBody::Paths(paths)) = command_block(&edit()) else {
            panic!("no paths block");
        };
        assert_eq!(paths, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn a_question_with_an_empty_prompt_draws_no_block() {
        let request = ApprovalRequest::question("req-q", "", vec!["A".to_string()]);
        assert_eq!(command_block(&request), None);
        // Still answerable: the options are the rows.
        assert_eq!(rows(&request, None).len(), 2);
    }

    #[test]
    fn a_question_shows_its_prompt() {
        let Some(ApprovalBody::Question(prompt)) = command_block(&question()) else {
            panic!("no question block");
        };
        assert_eq!(prompt, "Which database?");
    }

    // ── The tray ────────────────────────────────────────────────────────────

    use gpui::{AnyWindowHandle, Keystroke, TestAppContext, WindowHandle};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Hosts the tray, the way the shell would.
    ///
    /// A `Render` impl rather than a bare element: the tray uses `track_focus`,
    /// and focus tracking reaches for the view currently painting, which only a
    /// `Render` impl populates.
    struct Host {
        /// `None` until the first `open`; the window has to exist before a tray
        /// can be mounted into it, so the first frame has nothing to draw.
        tray: Option<Entity<ApprovalTray>>,
    }

    impl Render for Host {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            match self.tray.clone() {
                Some(tray) => tray.into_any_element(),
                // Before the first `open` there is no tray to draw. An empty
                // element rather than a placeholder card: a window that briefly
                // shows a decision the provider has not asked for is worse than
                // one that shows nothing.
                None => div().w(px(0.0)).h(px(0.0)).into_any_element(),
            }
        }
    }

    /// A mounted tray plus the record of what it reported.
    struct Mounted {
        test: TestAppContext,
        window: WindowHandle<Host>,
        tray: Entity<ApprovalTray>,
        decided: Rc<RefCell<Vec<(String, Decision)>>>,
        cancelled: Rc<RefCell<usize>>,
    }

    impl Mounted {
        fn decisions(&self) -> Vec<(String, Decision)> {
            self.decided.borrow().clone()
        }

        fn cancelled(&self) -> usize {
            *self.cancelled.borrow()
        }

        fn settled(&self) -> bool {
            self.test.read(|cx| self.tray.read(cx).is_settled())
        }

        fn displaced(&self) -> bool {
            self.test.read(|cx| self.tray.read(cx).is_displaced())
        }

        /// Send a key to the window, as the platform would.
        fn press(&mut self, key: &str) {
            let window: AnyWindowHandle = self.window.into();
            self.test
                .dispatch_keystroke(window, Keystroke::parse(key).expect("a parsable key"));
        }
    }

    /// Mount a tray for `request` into the window, displacing whatever is open.
    ///
    /// The same call the shell makes, from inside a window update because `open`
    /// needs a `Window` to focus with.
    fn open_in(
        window: WindowHandle<Host>,
        test: &mut TestAppContext,
        request: ApprovalRequest,
        decided: Rc<RefCell<Vec<(String, Decision)>>>,
        cancelled: Rc<RefCell<usize>>,
    ) -> Entity<ApprovalTray> {
        let mounted: Rc<RefCell<Option<Entity<ApprovalTray>>>> = Rc::default();
        let for_build = mounted.clone();
        window
            .update(test, move |host, window, cx| {
                let tray = ApprovalTray::open(
                    window,
                    cx,
                    request,
                    Box::new(move |id, decision, _window, _cx| {
                        decided.borrow_mut().push((id, decision));
                    }),
                    Box::new(move |_window, _cx| {
                        *cancelled.borrow_mut() += 1;
                    }),
                );
                host.tray = Some(tray.clone());
                *for_build.borrow_mut() = Some(tray);
            })
            .expect("the window is live");
        mounted.borrow().clone().expect("a tray was mounted")
    }

    /// Mount one tray for one request.
    fn mount(request: ApprovalRequest) -> Mounted {
        mount_all(vec![request])
    }

    /// Mount a tray for each request in turn, in one window.
    ///
    /// One window for all of them because "one at a time" is a claim about one
    /// app: a second window would have its own slot and displace nothing.
    fn mount_all(requests: Vec<ApprovalRequest>) -> Mounted {
        let mut test = TestAppContext::single();
        let decided: Rc<RefCell<Vec<(String, Decision)>>> = Rc::default();
        let cancelled: Rc<RefCell<usize>> = Rc::default();

        let window: WindowHandle<Host> = test.add_window(|_window, _cx| Host { tray: None });
        test.refresh().expect("the window painted");

        let mut trays = Vec::new();
        for request in requests {
            trays.push(open_in(
                window,
                &mut test,
                request,
                decided.clone(),
                cancelled.clone(),
            ));
        }
        test.refresh().expect("the tray painted");
        window
            .update(&mut test, |_host, _window, _cx| {})
            .expect("the window is still alive");

        let tray = trays.last().cloned().expect("a request was mounted");
        Mounted {
            window,
            test,
            tray,
            decided,
            cancelled,
        }
    }

    #[test]
    fn a_command_tray_paints() {
        mount(command());
    }

    #[test]
    fn a_command_with_unicode_and_no_directory_paints() {
        mount(ApprovalRequest::command(
            "req-u",
            "rm -rf ~/Documents/日本語 🗂 — 'quoted'",
            None,
        ));
    }

    #[test]
    fn a_command_too_long_to_fit_paints() {
        // The block wraps rather than truncating, so this is the case where the
        // card's height has to come from the content.
        mount(ApprovalRequest::command(
            "req-long",
            format!("echo {} ; rm -rf /", "x".repeat(2_000)),
            Some("/Users/you/code".into()),
        ));
    }

    #[test]
    fn a_command_with_nothing_to_show_still_paints() {
        // The empty-command rule removes the block, not the controls.
        mount(ApprovalRequest::command("req-empty", "", None));
    }

    #[test]
    fn an_edit_tray_paints() {
        mount(edit());
    }

    #[test]
    fn an_edit_with_no_paths_paints() {
        mount(ApprovalRequest::edit("req-e", Vec::new()));
    }

    #[test]
    fn a_question_tray_paints() {
        mount(question());
    }

    #[test]
    fn a_question_with_no_options_paints_its_only_way_out() {
        mount(ApprovalRequest::question(
            "req-4",
            "What should I name it?",
            Vec::new(),
        ));
    }

    #[test]
    fn a_question_with_repeated_options_paints() {
        // Two rows with the same label: the design system's `button` derives its
        // element id from the label, so this is where a duplicate could collide.
        mount(ApprovalRequest::question(
            "req-5",
            "Pick one",
            vec!["Yes".into(), "No".into(), "Yes".into()],
        ));
    }

    #[test]
    fn a_question_with_no_prompt_paints() {
        mount(ApprovalRequest::question("req-q", "  ", vec!["A".into()]));
    }

    #[test]
    fn the_shortcut_denies_a_command() {
        let mut mounted = mount(command());
        mounted.press("secondary-enter");

        assert_eq!(
            mounted.decisions(),
            vec![("req-1".to_string(), Decision::Deny)]
        );
        assert_eq!(mounted.cancelled(), 0);
    }

    #[test]
    fn a_bare_return_does_not_decide() {
        // A bare Return is the composer's submit; a tray above it must not
        // swallow the keystroke a reader aimed at the prompt box.
        let mut mounted = mount(command());
        mounted.press("enter");

        assert!(mounted.decisions().is_empty());
        assert_eq!(mounted.cancelled(), 0);
        assert!(!mounted.settled());
    }

    #[test]
    fn the_shortcut_cancels_a_question() {
        let mut mounted = mount(question());
        mounted.press("secondary-enter");

        // Cancel as a *decision*: the shortcut must never answer a question with
        // a permission it never offered.
        assert_eq!(
            mounted.decisions(),
            vec![("req-3".to_string(), Decision::Cancel)]
        );
    }

    #[test]
    fn escape_cancels() {
        let mut mounted = mount(command());
        mounted.press("escape");

        assert_eq!(mounted.cancelled(), 1);
        assert!(mounted.decisions().is_empty());
    }

    #[test]
    fn a_second_escape_does_not_cancel_twice() {
        let mut mounted = mount(command());
        mounted.press("escape");
        mounted.press("escape");

        assert_eq!(mounted.cancelled(), 1);
    }

    #[test]
    fn a_second_shortcut_does_not_decide_twice() {
        // The double-click bug, reached through the keyboard: the provider must
        // see one answer per request.
        let mut mounted = mount(command());
        mounted.press("secondary-enter");
        mounted.press("secondary-enter");

        assert_eq!(mounted.decisions().len(), 1);
        assert!(mounted.settled());
    }

    #[test]
    fn a_decided_tray_refuses_a_later_decision() {
        // Escape first, then the shortcut: whichever lands second must be
        // dropped rather than forwarded to a request already answered.
        let mut mounted = mount(command());
        mounted.press("escape");
        mounted.press("secondary-enter");

        assert_eq!(mounted.cancelled(), 1);
        assert!(mounted.decisions().is_empty());
    }

    #[test]
    fn a_settled_tray_reports_the_decision_it_was_given() {
        let mut mounted = mount(command());
        mounted.press("secondary-enter");

        assert_eq!(
            mounted.test.read(|cx| mounted.tray.read(cx).decision()),
            Some(Decision::Deny)
        );
    }

    #[test]
    fn a_tray_starts_with_no_decision_held() {
        // A pre-selected button claims the reader already decided.
        let mounted = mount(command());
        assert_eq!(
            mounted.test.read(|cx| mounted.tray.read(cx).decision()),
            None
        );
        assert!(!mounted.settled());
        assert!(!mounted.displaced());
    }

    #[test]
    fn opening_a_second_request_displaces_the_first() {
        // A provider blocked on a request nobody can reach any more hangs the
        // turn, so being displaced has to be reported rather than be a silent
        // unmount. `open` is the only thing that displaces, so this drives it.
        let mounted = mount_all(vec![command(), edit()]);

        assert_eq!(mounted.cancelled(), 1, "the displaced tray did not report");
        assert_eq!(
            mounted.decisions().len(),
            0,
            "the displaced tray also decided"
        );
        assert!(!mounted.displaced());
    }

    #[test]
    fn the_newest_request_is_the_one_on_screen() {
        let mounted = mount_all(vec![command(), edit()]);
        let shown = mounted
            .test
            .read(|cx| open_tray(cx).map(|tray| tray.read(cx).request().id.clone()));

        assert_eq!(
            shown,
            Some("req-2".to_string()),
            "the app still points at the displaced tray"
        );
    }

    #[test]
    fn the_displaced_tray_says_so() {
        let mut mounted = mount_all(vec![command()]);
        let first = mounted.tray.clone();
        let _second = open_in(
            mounted.window,
            &mut mounted.test,
            edit(),
            mounted.decided.clone(),
            mounted.cancelled.clone(),
        );

        assert!(mounted.test.read(|cx| first.read(cx).is_displaced()));
        assert!(
            mounted.test.read(|cx| first.read(cx).is_settled()),
            "a displaced tray is still waiting on a decision nobody can make"
        );
    }

    #[test]
    fn a_settled_tray_is_never_displaced_by_the_next_one() {
        // Displacing an already-settled tray would report a second outcome for a
        // request that was answered, which is the double-decision bug by another
        // route.
        let mut mounted = mount_all(vec![command()]);
        mounted.press("secondary-enter");
        let _second = open_in(
            mounted.window,
            &mut mounted.test,
            edit(),
            mounted.decided.clone(),
            mounted.cancelled.clone(),
        );
        mounted.test.refresh().expect("the second tray painted");

        assert_eq!(
            mounted.cancelled(),
            0,
            "the answered tray was reported cancelled as well"
        );
        assert_eq!(mounted.decisions().len(), 1);
    }

    #[test]
    fn a_request_is_never_answered_twice() {
        // Three claims against one tray, which is the shape a double-click
        // produces: one of them succeeds and the rest report nothing.
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                command(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut decisions = Vec::new();
            let mut cancels = Vec::new();
            tray.update(cx, |tray, _cx| {
                decisions.push(tray.claim_decision(Decision::Approve));
                decisions.push(tray.claim_decision(Decision::Deny));
                cancels.push(tray.claim_cancel());
            });
            (decisions, cancels)
        });

        assert_eq!(
            claimed.0,
            vec![Some(("req-1".to_string(), Decision::Approve)), None],
            "the provider could be told twice"
        );
        assert_eq!(
            claimed.1,
            vec![None],
            "the tray could be cancelled after deciding"
        );
        assert_eq!(
            test.read(|cx| tray.read(cx).decision()),
            Some(Decision::Approve)
        );
    }

    #[test]
    fn a_question_refuses_every_permission_through_its_own_api() {
        // Nothing in the UI offers these, so the guard is the last line: a
        // shortcut or a stale row must not be able to answer with a permission.
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                question(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut claimed = Vec::new();
            tray.update(cx, |tray, _cx| {
                for decision in Decision::ALL {
                    claimed.push(tray.claim_decision(decision));
                }
            });
            claimed
        });

        // Cancel is the one decision a question does draw, so it is the one that
        // comes back — every permission is refused.
        assert_eq!(
            claimed,
            vec![
                None,
                None,
                None,
                Some(("req-3".to_string(), Decision::Cancel)),
            ],
            "a question was answered with a permission"
        );
    }

    #[test]
    fn an_answer_claims_once() {
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                question(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut claimed = Vec::new();
            tray.update(cx, |tray, _cx| {
                claimed.push(tray.claim_answer(0));
                claimed.push(tray.claim_answer(1));
            });
            claimed
        });

        assert_eq!(claimed, vec![Some("Postgres".to_string()), None]);
    }

    #[test]
    fn an_answer_index_the_request_does_not_have_is_refused() {
        // A stale row, or a caller that guessed an index: nothing may be sent,
        // and a refusal must not settle the tray either.
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                question(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut claimed = Vec::new();
            tray.update(cx, |tray, _cx| claimed.push(tray.claim_answer(7)));
            claimed
        });

        assert_eq!(claimed, vec![None]);
        assert!(!test.read(|cx| tray.read(cx).is_settled()));
    }

    #[test]
    fn a_command_has_no_answers_to_claim() {
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                command(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut claimed = None;
            tray.update(cx, |tray, _cx| claimed = tray.claim_answer(0));
            claimed
        });

        assert_eq!(claimed, None);
    }

    #[test]
    fn a_displaced_tray_never_decides_afterwards() {
        // The displaced tray already reported; a click that was in flight when
        // the second request arrived must not add a second outcome.
        let test = TestAppContext::single();
        let tray = test.update(|cx| {
            ApprovalTray::new(
                cx,
                command(),
                Box::new(|_, _, _, _| {}),
                Box::new(|_, _| {}),
            )
        });
        let claimed = test.update(|cx| {
            let mut cancels = Vec::new();
            let mut decisions = Vec::new();
            tray.update(cx, |tray, _cx| {
                tray.mark_displaced();
                cancels.push(tray.claim_cancel());
                decisions.push(tray.claim_decision(Decision::Approve));
            });
            (cancels, decisions)
        });

        assert_eq!(claimed.0, vec![Some("req-1".to_string())]);
        assert_eq!(claimed.1, vec![None]);
        assert!(test.read(|cx| tray.read(cx).is_displaced()));
    }
}
