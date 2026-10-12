//! The conversation transcript.
//!
//! Everything a turn produced, in the order it was produced: the prose the
//! model wrote, the tool traffic folded into a disclosure, the reply still
//! streaming in, and whatever the provider is blocked on.
//!
//! Three decisions are borrowed rather than re-made here, because each of them
//! is load-bearing somewhere else and a second copy is a second place to be
//! wrong:
//!
//! - **What is prose and what is activity** belongs to
//!   `TranscriptActivityGroupingPolicy`. This file separates turns and hands
//!   each one over.
//! - **Whether the view follows the bottom** belongs to
//!   `ConversationScrollPolicy`. This file only supplies the geometry GPUI
//!   measured.
//! - **The reader's own prompts** are lifted out before grouping, so the policy
//!   never sees a `user` message and never has to reason about role.
//!
//! One structural constraint shapes the rest of the file: this is a function,
//! not a `Render` entity, so it owns no per-thread state of its own. The scroll
//! handle and the disclosure flags live in a thread-local — the same mechanism
//! the theme uses, because GPUI renders on one thread. Neither is `Send`, which
//! is why they cannot become a `gpui::Global` either. When `AppState` grows the
//! per-thread UI state the shell already owns, these move there.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use fx_agent::conversation::state::RiskLevelSerde;
use fx_agent::conversation::{PendingApproval, PendingQuestion, QuestionOption};
use fx_core::Timestamp;
use fx_core::policy::conversation_scroll::{ConversationScrollPolicy, ScrollMetrics};
use fx_core::policy::transcript_grouping::{
    QUESTION_TOOL_NAME, TranscriptActivityGroupingPolicy, TranscriptActivitySegment,
};
use fx_core::{ConversationImageAssetReference, ConversationMessage, MessageContent, MessageRole};
use fx_design::components::surface::banner;
use fx_design::components::{
    BadgeTone, DividerAxis, badge, divider_with_axis, empty_state, panel, spinner_arc,
    typing_indicator,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{
    BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, Typography, WithAlpha, global,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, CursorStyle, FocusHandle, KeyDownEvent, ScrollHandle, SharedString, Window,
    div, px,
};
use uuid::Uuid;

/// Element id of the transcript's scroll viewport.
///
/// Stable across renders on purpose. The scroll offset is keyed by the element's
/// identity, so a revision-dependent id would throw away the reader's position
/// on every streamed delta.
const SCROLL_ID: &str = "transcript.scroll";
const COLUMN_SELECTOR: &str = "transcript.column";
const EMPTY_SELECTOR: &str = "transcript.empty";
const USER_SELECTOR: &str = "transcript.user";
const ASSISTANT_SELECTOR: &str = "transcript.assistant";

/// The name a result carries when the transcript never saw the call it belongs
/// to.
///
/// A provider can replay a result after a resume, and a restored transcript can
/// arrive with its calls trimmed. The row is still worth showing: a failure with
/// no call above it is exactly what the reader needs to see.
const ORPHAN_TOOL: &str = "Tool";

/// How many characters a tool's detail block shows before it is cut off.
const DETAIL_CHARACTER_LIMIT: usize = 2_000;

/// How many characters a one-line tool summary shows before it is elided.
const SUMMARY_CHARACTER_LIMIT: usize = 100;

/// The caret's width. A hairline would disappear at some zoom levels and a
/// wider bar stops reading as a cursor.
const CARET_WIDTH: f32 = 2.0;

/// Width of the gutter a question's option number sits in, so option text lines
/// up under itself rather than under its own marker.
const OPTION_MARKER_WIDTH: f32 = 16.0;

/// Opacity of the caret on the half of its blink it is faded.
///
/// Not a theme token: a caret is an animation frame, and the animation system
/// owns its own constants.
const CARET_FADED_OPACITY: f32 = 0.25;

/// How many rendered passes make up one half of the caret's blink.
///
/// With no animation clock, passes are the only unit available. Eight is a
/// compromise: fast enough that the caret reads as blinking, slow enough that a
/// stream does not turn this into a frame per delta.
const CARET_BLINK_PASSES: u32 = 8;

// ── Per-thread UI state ────────────────────────────────────────────────────

/// State this view owns because nothing above it can hold it yet.
#[derive(Default)]
struct ViewMemory {
    /// One scroll handle per conversation, so a thread keeps its position when
    /// the reader switches away and back.
    scrolls: HashMap<Uuid, ScrollHandle>,
    /// Whether each activity group is open, keyed by its stable group key.
    ///
    /// Keys carry the conversation, so pruning one thread's groups cannot close
    /// another's.
    disclosures: HashMap<String, bool>,
    /// The content revision each conversation's disclosure map was last pruned
    /// at. Without this, a long-lived thread accumulates one entry per group it
    /// has ever shown.
    pruned_at: HashMap<Uuid, (u64, u64)>,
    /// The blink phase the last caret frame was requested for.
    blink_requested_at: HashMap<Uuid, u32>,
    /// One focus handle per conversation's viewport, for keyboard paging.
    focuses: HashMap<Uuid, FocusHandle>,
    /// Advances once per pass.
    ///
    /// GPUI 0.2.2 has no animation clock, so nothing could drive the spinner or
    /// the typing indicator on a timer. Counting passes instead is honest about
    /// what it is: motion that only happens while there is something to report.
    frame: u32,
}

impl ViewMemory {
    fn next_frame(&mut self) -> u32 {
        self.frame = self.frame.wrapping_add(1);
        self.frame
    }
}

thread_local! {
    static MEMORY: RefCell<ViewMemory> = RefCell::new(ViewMemory::default());
}

fn scroll_handle(conversation: Uuid) -> ScrollHandle {
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        memory
            .scrolls
            .entry(conversation)
            .or_insert_with(ScrollHandle::new)
            .clone()
    })
}

fn is_disclosed(key: &str) -> bool {
    MEMORY.with(|memory| {
        memory
            .borrow()
            .disclosures
            .get(key)
            .copied()
            .unwrap_or(false)
    })
}

fn toggle_disclosure(key: &str) {
    MEMORY.with(|memory| {
        let open = memory
            .borrow()
            .disclosures
            .get(key)
            .copied()
            .unwrap_or(false);
        memory
            .borrow_mut()
            .disclosures
            .insert(key.to_string(), !open);
    });
}

/// Forget the groups a conversation no longer has.
///
/// Keyed on the revisions rather than run every pass: pruning is a map rebuild,
/// and a stream produces a revision on every delta.
fn prune_disclosures(
    conversation: Uuid,
    message_revision: u64,
    streaming_revision: u64,
    live: &HashSet<String>,
) {
    let prefix = format!("{conversation}:");
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        if memory.pruned_at.get(&conversation).copied()
            == Some((message_revision, streaming_revision))
        {
            return;
        }
        memory
            .pruned_at
            .insert(conversation, (message_revision, streaming_revision));
        // Only this conversation's keys are judged, so another thread's open
        // groups survive being filtered.
        memory
            .disclosures
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
    });
}

// ── The row model ──────────────────────────────────────────────────────────

/// One thing the transcript shows, resolved from a contiguous run of messages.
enum Row<'a> {
    /// Prose, a code block, an attachment, or a question — rendered as written.
    Message(&'a ConversationMessage),
    /// Tool traffic, collapsed into a group the reader can open.
    Activity(ActivityGroup<'a>),
}

/// A disclosure over one burst of tool traffic.
struct ActivityGroup<'a> {
    /// Stable across renders and across the transcript growing above it, so an
    /// expansion survives new messages landing.
    key: String,
    calls: Vec<ToolCall<'a>>,
    summary: ActivitySummary,
    /// Whether this is the tail of a turn that is still running.
    is_active: bool,
}

/// One tool call and, when it arrived, its result.
struct ToolCall<'a> {
    id: String,
    name: &'a str,
    input: &'a str,
    output: Option<&'a str>,
    is_error: bool,
    /// Whether a result block was attached to this call.
    ///
    /// Not the same as "the call is not running any more": a successful result
    /// that said nothing is never recorded, so a finished call can sit here with
    /// no result at all. The engine's completed-id memory settles that case.
    result_recorded: bool,
}

/// What a group's header reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ActivitySummary {
    calls: usize,
    edits: usize,
    failures: usize,
    /// Elapsed wall time across the group, when the timestamps say so.
    seconds: Option<u64>,
}

/// Split the transcript into the rows it should show.
///
/// Turns are separated by the reader's own prompts first: an assistant's reply is
/// the unit the activity grouping is defined over, and two replies either side of
/// a prompt are not one reply.
fn build_rows<'a>(
    messages: &[&'a ConversationMessage],
    turn_is_running: bool,
    conversation: Uuid,
) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    let mut turn: Vec<&'a ConversationMessage> = Vec::new();

    for message in messages {
        if message.role == MessageRole::User {
            push_turn(&mut rows, &mut turn, false, conversation);
            rows.push(Row::Message(message));
        } else {
            turn.push(message);
        }
    }
    push_turn(&mut rows, &mut turn, turn_is_running, conversation);
    rows
}

/// Hand one assistant turn to the grouping policy and take the rows it names.
fn push_turn<'a>(
    rows: &mut Vec<Row<'a>>,
    turn: &mut Vec<&'a ConversationMessage>,
    is_running: bool,
    conversation: Uuid,
) {
    if turn.is_empty() {
        return;
    }
    let turn = std::mem::take(turn);

    // The policy borrows a slice of messages and the engine only hands out an
    // iterator over a `VecDeque`, so the turn has to be materialized for it. It
    // is bounded by a turn rather than by the transcript, and the alternative —
    // re-deciding what counts as activity inside the view — is the rule that
    // already had to be extracted into the policy once.
    let owned: Vec<ConversationMessage> = turn.iter().map(|message| (*message).clone()).collect();
    let segments = TranscriptActivityGroupingPolicy::segments(&owned);

    // The segments are a partition of `owned` in order, so walking them with a
    // cursor recovers the matching borrowed messages without matching on ids.
    let mut cursor = 0;
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.into_iter().enumerate() {
        let borrowed = &turn[cursor..cursor + segment.messages().len()];
        cursor += segment.messages().len();
        match segment {
            TranscriptActivitySegment::Narrative { .. } => {
                rows.extend(borrowed.iter().map(|message| Row::Message(message)));
            }
            TranscriptActivitySegment::Activity { .. } => {
                // Only the last segment of a running turn is live. Anything else
                // has already been overtaken by what follows it, and holding it
                // open would leave a spinner running on settled work.
                rows.push(Row::Activity(ActivityGroup {
                    key: activity_key(conversation, borrowed, index),
                    calls: tool_calls(borrowed),
                    summary: summarize(borrowed),
                    is_active: is_running && index == last,
                }));
            }
        }
    }
}

/// A key that survives the transcript growing above the group.
///
/// The conversation, the first message's id, and the group's position inside the
/// turn. The id alone would collide when one turn produced two groups of the
/// same tool; the conversation keeps one thread's groups from closing another's.
fn activity_key(conversation: Uuid, messages: &[&ConversationMessage], index: usize) -> String {
    let first = messages
        .first()
        .map(|message| message.id)
        .unwrap_or(Uuid::nil());
    format!("{conversation}:work:{first}:{index}")
}

/// Pair each call with its result.
fn tool_calls<'a>(messages: &[&'a ConversationMessage]) -> Vec<ToolCall<'a>> {
    let mut calls: Vec<ToolCall<'a>> = Vec::new();
    // Calls with no id cannot be keyed, and two of them would collide on an empty
    // string. They keep their own row instead, which is what the id is for.
    let mut index_by_id: HashMap<&str, usize> = HashMap::new();

    for message in messages {
        for block in &message.content {
            match block {
                MessageContent::ToolUse { id, name, input } => {
                    if !id.is_empty() {
                        index_by_id.insert(id.as_str(), calls.len());
                    }
                    calls.push(ToolCall {
                        id: id.clone(),
                        name,
                        input,
                        output: None,
                        is_error: false,
                        result_recorded: false,
                    });
                }
                MessageContent::ToolResult {
                    id,
                    content,
                    is_error,
                } => {
                    let Some(position) = index_by_id.get(id.as_str()).copied() else {
                        calls.push(ToolCall {
                            id: id.clone(),
                            name: ORPHAN_TOOL,
                            input: "",
                            output: Some(content),
                            is_error: *is_error,
                            result_recorded: true,
                        });
                        continue;
                    };
                    let call = &mut calls[position];
                    call.output = Some(content);
                    call.is_error = *is_error;
                    call.result_recorded = true;
                }
                _ => {}
            }
        }
    }
    calls
}

fn summarize(messages: &[&ConversationMessage]) -> ActivitySummary {
    let mut summary = ActivitySummary::default();
    let mut earliest: Option<Timestamp> = None;

    for message in messages {
        for block in &message.content {
            match block {
                MessageContent::ToolUse { name, .. } => {
                    // The question tool is not background work — it is the
                    // transcript asking the reader something — and the grouping
                    // policy already keeps it out of the group, so it cannot
                    // reach this count. Skipping it would be a second rule for a
                    // decision that is not this file's.
                    if name.eq_ignore_ascii_case(QUESTION_TOOL_NAME) {
                        continue;
                    }
                    summary.calls += 1;
                    if name.eq_ignore_ascii_case("Edit") || name.eq_ignore_ascii_case("Write") {
                        summary.edits += 1;
                    }
                }
                MessageContent::ToolResult { is_error: true, .. } => summary.failures += 1,
                _ => {}
            }
        }
        earliest = Some(match earliest {
            None => message.timestamp,
            Some(seen) => seen.min(message.timestamp),
        });
    }

    summary.seconds = match (earliest, messages.last()) {
        // A sub-second burst says nothing useful beside the call count.
        (Some(start), Some(last)) => {
            let elapsed = last
                .timestamp
                .signed_duration_since(start)
                .num_seconds()
                .max(0) as u64;
            (elapsed > 0).then_some(elapsed)
        }
        _ => None,
    };
    summary
}

// ── Entry point ────────────────────────────────────────────────────────────

/// Render one conversation's transcript.
pub fn view(conversation: &crate::Conversation, window: &mut Window, cx: &mut App) -> AnyElement {
    let theme = global::current();
    let handle = scroll_handle(conversation.id);
    pin_to_bottom(&handle);

    let messages: Vec<&ConversationMessage> = conversation.messages().collect();
    let rows = build_rows(&messages, conversation.is_busy(), conversation.id);

    let live: HashSet<String> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Activity(group) => Some(group.key.clone()),
            Row::Message(_) => None,
        })
        .collect();
    prune_disclosures(
        conversation.id,
        conversation.message_revision(),
        conversation.streaming_revision(),
        &live,
    );

    let frame = MEMORY.with(|memory| memory.borrow_mut().next_frame());
    request_caret_frame(conversation, frame, window);
    let streaming = streaming_tail(conversation, frame);
    let is_empty = rows.is_empty() && streaming.is_none() && conversation.error().is_none();
    let focus = viewport_focus(conversation.id, cx);
    let for_keys = handle.clone();

    let mut column = div()
        .debug_selector(|| COLUMN_SELECTOR.to_string())
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .max_w(px(Layout::READABLE_CONTENT_WIDTH))
        .px(px_len(Spacing::XXL))
        .pt(px_len(Spacing::XXL))
        .pb(px_len(Spacing::XXL));

    if is_empty {
        // Give the empty state the same viewport as a populated conversation.
        // Padding alone leaves it stranded at the top of a tall window.
        column = column
            .h_full()
            .justify_center()
            .child(empty_transcript_notice());
    }

    for row in &rows {
        column = column.child(match row {
            Row::Message(message) => message_row(message),
            Row::Activity(group) => activity_group(group, conversation, frame),
        });
    }

    if let Some(tail) = streaming {
        column = column.child(tail);
    }
    if let Some(error) = conversation.error() {
        column = column.child(error_banner(error));
    }

    div()
        .w_full()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .bg(Colors::content_bg(&theme))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                // `.id` is what upgrades a `Div` to `Stateful<Div>`; in GPUI
                // 0.2.2 scrolling is an interaction, and interactions live on
                // the stateful element.
                .id(SCROLL_ID)
                .debug_selector(|| SCROLL_ID.to_string())
                .overflow_y_scroll()
                // Reading the offset back through the handle is what makes the
                // follow-the-bottom decision observable at all: without it this
                // element could scroll but nothing could tell that it had.
                .track_scroll(&handle)
                // `track_focus` also makes the element focusable, which is what
                // lets the key handler below be reached at all.
                .track_focus(&focus)
                .on_key_down(move |event, window, cx| {
                    if !page_with_keyboard(&for_keys, event) {
                        // Not a key this viewport claims. Stopping propagation
                        // anyway would swallow Tab and every shortcut while it
                        // had focus.
                        return;
                    }
                    // Claimed rather than passed on: an arrow key that also
                    // reached the composer would move the caret while the
                    // transcript scrolled.
                    cx.stop_propagation();
                    // Writing the offset is not enough on its own — nothing has
                    // asked for a frame yet.
                    window.refresh();
                })
                .w_full()
                .flex()
                // The column keeps its natural height as history grows. Flex's
                // default cross-axis stretch would squash rows to the viewport.
                .items_start()
                .justify_center()
                .child(column),
        )
        .child(pending_prompts(conversation))
        .into_any_element()
}

/// The focus handle for a conversation's viewport.
///
/// One per thread and kept for the life of the process: a handle made fresh each
/// pass would orphan the focused one, and focus would be lost on every repaint.
fn viewport_focus(conversation: Uuid, cx: &mut App) -> FocusHandle {
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        let mut handle = cx.focus_handle();
        // Reachable from the keyboard at all. Without this the viewport is
        // focusable in principle and unreachable in practice.
        handle.tab_stop = true;
        memory.focuses.entry(conversation).or_insert(handle).clone()
    })
}

/// Move the viewport a page or a line at a time.
///
/// GPUI 0.2.2 gives a scrollable element no key handling whatsoever, so the
/// arrows and page keys go nowhere unless something claims them. The original
/// inherited this from `NSScrollView` without writing a line for it, so leaving
/// it out would drop a capability rather than a convenience.
fn page_with_keyboard(handle: &ScrollHandle, event: &KeyDownEvent) -> bool {
    let offset = handle.offset();
    match event.keystroke.key.as_str() {
        "home" => handle.set_offset(gpui::point(offset.x, px(0.0))),
        "end" => handle.set_offset(gpui::point(offset.x, -handle.max_offset().height)),
        _ => {
            let viewport = f64::from(handle.bounds().size.height);
            // A viewport that has not been laid out yet has no height, so a page
            // step derived from it would be nonsense.
            if viewport <= 1.0 {
                return false;
            }
            let line = f64::from(Typography::body_point_size(&global::current())) * 3.0;
            let page = viewport * 0.9;
            let step = match event.keystroke.key.as_str() {
                "pagedown" => page,
                "pageup" => -page,
                "down" => line,
                "up" => -line,
                _ => return false,
            };
            let next = f64::from(offset.y) - step;
            if (next - f64::from(offset.y)).abs() < 1.0 {
                return false;
            }
            handle.set_offset(gpui::point(offset.x, gpui::px(next as f32)));
        }
    }
    true
}

/// Keep the caret blinking while a reply arrives.
///
/// GPUI 0.2.2 has no animation clock, and `Window::request_animation_frame` is
/// not usable from here: it reaches for the entity currently painting, which
/// unwraps a stack only populated during prepaint and paint — and a render
/// function runs before either. Registering a next-frame callback is the one
/// animation hook that is safe to call from a render pass.
///
/// The request is keyed on the blink *phase*, not on every pass. The transcript
/// is rebuilt from scratch on each render, and the original went to some length
/// to avoid exactly that per delta, so asking for a frame per pass would undo
/// the saving. Keying on the phase bounds it to one extra frame every
/// `CARET_BLINK_PASSES` passes, however fast the stream runs.
fn request_caret_frame(conversation: &crate::Conversation, frame: u32, window: &mut Window) {
    if !conversation.is_streaming() {
        return;
    }
    let phase = blink_phase(frame);
    let wanted = MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        if memory.blink_requested_at.get(&conversation.id) == Some(&phase) {
            return false;
        }
        memory.blink_requested_at.insert(conversation.id, phase);
        true
    });
    if wanted {
        // `refresh` forces the view to re-render rather than repaint its cached
        // tree, which is the point: the caret's opacity is decided here.
        window.on_next_frame(move |_window, cx| cx.refresh_windows());
    }
}

/// Whether the caret is drawn solid or faded, as a phase rather than a boolean.
fn blink_phase(frame: u32) -> u32 {
    (frame / CARET_BLINK_PASSES) % 2
}

/// Decide whether the view should follow the bottom, and ask it to if so.
///
/// The geometry is whatever the last layout measured, because a scroll offset
/// cannot be computed before the frame that produces it. GPUI measures a
/// negative offset going down, so the sign is flipped back before the policy
/// sees it.
fn pin_to_bottom(handle: &ScrollHandle) {
    let offset = handle.offset();
    let max = handle.max_offset();
    let pinned = ConversationScrollPolicy::is_pinned(ScrollMetrics {
        offset: -f64::from(offset.y) as f32,
        max_offset: f64::from(max.height) as f32,
    });
    if pinned {
        // A flag rather than a direct write: the next layout pass consumes it and
        // clamps against the geometry it is laying out, which is the only place
        // the new bottom is known.
        handle.scroll_to_bottom();
    }
}

// ── Messages ───────────────────────────────────────────────────────────────

/// One transcript row. A bubble for the reader's own prompts, prose for the rest.
fn message_row(message: &ConversationMessage) -> AnyElement {
    let theme = global::current();
    let is_user = message.role == MessageRole::User;

    let mut blocks = Vec::new();
    for block in &message.content {
        if let Some(element) = content_block(block) {
            blocks.push(element);
        }
    }
    // A message whose blocks all render to nothing still gets a row. A restored
    // transcript can carry one, and dropping it silently is indistinguishable
    // from losing it.
    if blocks.is_empty() {
        blocks.push(
            div()
                .fx_caption()
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from("Empty message"))
                .into_any_element(),
        );
    }

    let mut bubble = div().min_w_0().flex().flex_col().gap(px_len(Spacing::SM));
    if is_user {
        bubble = bubble
            .debug_selector(|| USER_SELECTOR.to_string())
            .px(px_len(Spacing::XL))
            .py(px_len(Spacing::MD))
            .max_w(px(Layout::USER_MESSAGE_MAX_WIDTH))
            .rounded(px_len(Radii::XL))
            .bg(Colors::accent_muted(&theme))
            // The accent at low strength, which is a wash over the muted fill
            // rather than a second surface colour.
            .border(px_len(BorderWidth::HAIRLINE))
            .border_color(Colors::accent(&theme).with_alpha(0.2));
    } else {
        bubble = bubble
            .debug_selector(|| ASSISTANT_SELECTOR.to_string())
            .w_full()
            .py(px_len(Spacing::XS));
    }
    let bubble = bubble.children(blocks);

    // The reader's prompt sits against the trailing edge and the model's answer
    // against the leading one, which is what makes a long exchange readable
    // without any role label on either side.
    let column = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .pb(px_len(Spacing::XXL));
    if is_user {
        column.items_end().child(bubble).into_any_element()
    } else {
        column.items_start().child(bubble).into_any_element()
    }
}

/// One content block, or `None` when it has nothing to show.
fn content_block(block: &MessageContent) -> Option<AnyElement> {
    match block {
        MessageContent::Text { text } => {
            if text.trim().is_empty() {
                return None;
            }
            Some(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .fx_body()
                    .child(SharedString::from(text.clone()))
                    .into_any_element(),
            )
        }
        MessageContent::Code { language, code } => {
            let trimmed = language.trim();
            let label = if trimmed.is_empty() {
                "Code".to_string()
            } else {
                trimmed.to_ascii_uppercase()
            };
            Some(mono_panel(&label, code, BlockTone::Standard))
        }
        MessageContent::Image { data, mime_type } => Some(attachment_notice(&format!(
            "Image attachment ({mime_type}, {} bytes)",
            data.len()
        ))),
        MessageContent::ImageAsset(reference) => Some(asset_notice(reference)),
        MessageContent::ToolUse { name, input, .. } => {
            if name.eq_ignore_ascii_case(QUESTION_TOOL_NAME) {
                Some(question_card(input))
            } else {
                Some(inline_tool_call(name, input))
            }
        }
        MessageContent::ToolResult {
            content, is_error, ..
        } => Some(inline_tool_result(content, *is_error)),
    }
}

/// A tool call rendered inside a narrative row, because it shares a message with
/// prose and collapsing it would bury the words the model just said.
fn inline_tool_call(name: &str, input: &str) -> AnyElement {
    let theme = global::current();
    let mut block = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(tool_glyph(name))
                .child(
                    div()
                        .fx_role(TextRole::CaptionMedium)
                        .text_color(Colors::accent_secondary(&theme))
                        .child(SharedString::from(name.to_string())),
                ),
        );
    if !input.is_empty() && input != "{}" {
        block = block.child(mono_panel("Input", input, BlockTone::Standard));
    }
    block.into_any_element()
}

/// A tool result rendered inside a narrative row.
///
/// An error result is always recorded — that is the whole point of recording it
/// — so it has to be noticeable even when it is the only thing in its row. The
/// tint is the wash token rather than the hue at low alpha, because that is what
/// the rest of the app uses for a failure.
fn inline_tool_result(content: &str, is_error: bool) -> AnyElement {
    let theme = global::current();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .rounded(px_len(Radii::MD))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .when(is_error, |this| {
            this.bg(Colors::error_muted(&theme))
                .border_l(px_len(BorderWidth::STRONG))
                .border_color(Colors::error(&theme))
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(if is_error {
                    Icon::EXCLAMATIONMARK_TRIANGLE_FILL.render(
                        IconSize::Regular,
                        Colors::error(&theme),
                        &theme,
                    )
                } else {
                    Icon::CHECKMARK_CIRCLE_FILL.render(
                        IconSize::Regular,
                        Colors::success(&theme),
                        &theme,
                    )
                })
                .child(
                    div()
                        .fx_role(TextRole::CaptionMedium)
                        .child(SharedString::from(if is_error {
                            "Tool failed"
                        } else {
                            "Tool result"
                        })),
                ),
        )
        .child(if is_error {
            mono_panel("Output", content, BlockTone::Failure)
        } else {
            mono_panel("Output", content, BlockTone::Standard)
        })
        .into_any_element()
}

/// The transcript's own picture of an inline image.
///
/// Decoding needs an image cache and a downsample step, which belongs with a
/// message-image view rather than here. Until that lands the row says what it
/// is instead of drawing an empty box, because a blank rectangle is
/// indistinguishable from a failed decode.
fn attachment_notice(detail: &str) -> AnyElement {
    let theme = global::current();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .rounded(px_len(Radii::MD))
        .px(px_len(Spacing::MD))
        .py(px_len(Spacing::SM))
        .bg(Colors::bg_surface(&theme))
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .child(Icon::PHOTO.render(IconSize::Regular, Colors::fg_tertiary(&theme), &theme))
        .child(
            div()
                .min_w_0()
                .fx_caption()
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from(detail.to_string())),
        )
        .into_any_element()
}

fn asset_notice(reference: &ConversationImageAssetReference) -> AnyElement {
    attachment_notice(&format!(
        "Image attachment ({}, {} bytes)",
        reference.mime_type, reference.byte_count
    ))
}

/// An empty transcript says so, rather than showing nothing.
///
/// A blank column is indistinguishable from a transcript that failed to load.
fn empty_transcript_notice() -> AnyElement {
    let theme = global::current();
    div()
        .debug_selector(|| EMPTY_SELECTOR.to_string())
        .w_full()
        .min_w_0()
        .text_center()
        .child(empty_state(
            Icon::BUBBLE_LEFT_AND_BUBBLE_RIGHT.render(
                IconSize::Illustration,
                Colors::fg_tertiary(&theme),
                &theme,
            ),
            "Start a conversation",
            "Ask a question, explore an idea, or start building.",
        ))
        .into_any_element()
}

// ── The live tail ──────────────────────────────────────────────────────────

/// The reply still being written, or the indicator that stands in for one before
/// any of it has arrived. `None` when there is nothing live to show.
fn streaming_tail(conversation: &crate::Conversation, frame: u32) -> Option<AnyElement> {
    let theme = global::current();
    let text = conversation.streaming_text();

    if !text.trim().is_empty() {
        let body = Typography::style(TextRole::Body, &theme);
        return Some(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .py(px_len(Spacing::XS))
                .pb(px_len(Spacing::XXL))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px_len(Spacing::XXS))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .fx_body()
                                .child(SharedString::from(text.to_string())),
                        )
                        // The caret is exactly one body line tall. GPUI has no
                        // caret, and a bar taller than the line it sits in reads
                        // as a stray rule rather than as a cursor.
                        .child(
                            div()
                                .w(px(CARET_WIDTH))
                                .flex_shrink_0()
                                .h(px(body.size * TextRole::Body.line_height_multiplier()))
                                .rounded_full()
                                .bg(Colors::accent(&theme))
                                .opacity(if blink_phase(frame) == 0 {
                                    1.0
                                } else {
                                    CARET_FADED_OPACITY
                                }),
                        ),
                )
                .into_any_element(),
        );
    }

    // Waiting on the reader is not working, and saying otherwise would claim
    // progress that is not happening.
    if !conversation.is_busy() || conversation.is_waiting_for_user() {
        return None;
    }

    Some(
        div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px_len(Spacing::SM))
            .pb(px_len(Spacing::XXL))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px_len(Spacing::MD))
                    .py(px_len(Spacing::SM))
                    .child(typing_indicator(typing_phase(frame), false))
                    .child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child(SharedString::from(conversation.phase().label())),
                    ),
            )
            .into_any_element(),
    )
}

fn error_banner(message: &str) -> AnyElement {
    let theme = global::current();
    // `fg` rather than the error hue: the invariant the design system pins is
    // that banner text clears AA on every `*_muted` fill, and the hue does not.
    div()
        .mt(px_len(Spacing::SM))
        .child(banner(
            Colors::error_muted(&theme),
            Colors::fg(&theme),
            div()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::XXS))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px_len(Spacing::SM))
                        .child(Icon::EXCLAMATIONMARK_TRIANGLE_FILL.render(
                            IconSize::Regular,
                            Colors::error(&theme),
                            &theme,
                        ))
                        .child(
                            div()
                                .fx_caption()
                                .child(SharedString::from("The turn failed")),
                        ),
                )
                .child(
                    div()
                        .fx_caption()
                        .text_color(Colors::fg_secondary(&theme))
                        .child(SharedString::from(message.to_string())),
                ),
        ))
        .into_any_element()
}

// ── The activity group ─────────────────────────────────────────────────────

/// A collapsible group of tool traffic.
fn activity_group(
    group: &ActivityGroup<'_>,
    conversation: &crate::Conversation,
    frame: u32,
) -> AnyElement {
    let theme = global::current();
    // A group being worked on is forced open, as in the original: a reader
    // watching a turn is watching it work, not reading its history. The toggle
    // still records a choice, which takes effect once the turn settles.
    let expanded = group.is_active || is_disclosed(&group.key);

    let chevron = if expanded {
        Icon::CHEVRON_DOWN
    } else {
        Icon::CHEVRON_RIGHT
    }
    .render(IconSize::Micro, Colors::fg_quaternary(&theme), &theme);
    let status: AnyElement = if group.is_active {
        spinner_arc(Colors::accent(&theme), spin_degrees(frame)).into_any_element()
    } else {
        Icon::TERMINAL.render(IconSize::Small, Colors::fg_tertiary(&theme), &theme)
    };

    let failures: AnyElement = if group.summary.failures > 0 {
        badge(
            SharedString::from(failure_label(group.summary.failures)),
            BadgeTone::Error,
            true,
        )
        .into_any_element()
    } else {
        div().w(px(0.0)).h(px(0.0)).into_any_element()
    };

    let header = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .px(px_len(Spacing::SM))
        .py(px_len(Spacing::SM))
        .rounded(px_len(Radii::SM))
        .cursor(CursorStyle::PointingHand)
        .child(chevron)
        .child(status)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .truncate()
                .child(SharedString::from(group_title(group))),
        )
        .child(failures)
        .id(SharedString::from(format!("activity:{}", group.key)))
        .hover(|style| style.fx_background(Colors::bg_hover(&theme)))
        .on_click({
            let key = group.key.clone();
            move |_, _window, cx| {
                toggle_disclosure(&key);
                // This view is a function, so there is no entity to notify.
                // Refreshing the window is the same gesture one level up, and it
                // only happens when a reader opens something.
                cx.refresh_windows();
            }
        });

    let mut column = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .pb(px_len(Spacing::MD))
        .child(header);

    if expanded {
        let mut details = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::SM))
            .pl(px_len(Spacing::XXL))
            .pr(px_len(Spacing::SM))
            .py(px_len(Spacing::SM))
            // The leading rule is what ties a call to the group it belongs to.
            // Without it a long list of rows reads as more transcript.
            .border_l(px_len(BorderWidth::HAIRLINE))
            .border_color(Colors::border_subtle(&theme));
        for call in &group.calls {
            details = details.child(tool_row(call, group.is_active, conversation, frame));
        }
        column = column.child(details);
    }

    column.into_any_element()
}

/// One call inside a group, with its state at the trailing edge.
fn tool_row(
    call: &ToolCall<'_>,
    group_is_active: bool,
    conversation: &crate::Conversation,
    frame: u32,
) -> AnyElement {
    let theme = global::current();
    // A successful result that said nothing is never recorded as a row, so the
    // transcript cannot tell a finished call from a running one on its own. The
    // engine's completed-id memory is what closes that gap.
    let finished = call.result_recorded || conversation.has_completed_tool(&call.id);
    let running = !finished && group_is_active;

    let status: AnyElement = if call.is_error {
        Icon::XMARK_CIRCLE_FILL.render(IconSize::Small, Colors::error(&theme), &theme)
    } else if running {
        spinner_arc(Colors::accent(&theme), spin_degrees(frame)).into_any_element()
    } else {
        Icon::CHECKMARK.render(IconSize::Micro, Colors::success(&theme), &theme)
    };

    let title = div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .py(px_len(Spacing::XXXS))
        .child(div().flex_shrink_0().child(tool_glyph(call.name)))
        .child(
            div()
                .fx_role(TextRole::CaptionMedium)
                .flex_shrink_0()
                .text_color(Colors::fg_secondary(&theme))
                .whitespace_nowrap()
                .child(SharedString::from(tool_title(call))),
        )
        .child(if let Some(detail) = tool_detail(call) {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_right()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from(detail))
                .into_any_element()
        } else {
            div().flex_1().min_w_0().into_any_element()
        })
        .child(div().flex_shrink_0().child(status));

    let mut column = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XXS))
        .child(title);

    // A failure is shown whole rather than summarized. The one line is what makes
    // a row fit; the failure is the one thing in the group the reader came for,
    // and truncating it is how "permission denied" becomes "perm…".
    if let (true, Some(output)) = (call.is_error, call.output) {
        column = column.child(mono_panel("Failure", output, BlockTone::Failure));
    }

    column.into_any_element()
}

/// The glyph that names what kind of call this is.
fn tool_glyph(name: &str) -> AnyElement {
    let theme = global::current();
    let icon = if name.eq_ignore_ascii_case("Read") {
        Icon::DOC_TEXT
    } else if name.eq_ignore_ascii_case("Edit") || name.eq_ignore_ascii_case("Write") {
        Icon::PENCIL_LINE
    } else if name.eq_ignore_ascii_case("Grep") || name.eq_ignore_ascii_case("Glob") {
        Icon::MAGNIFYINGGLASS
    } else if matches!(
        name.to_ascii_lowercase().as_str(),
        "bash" | "command" | "shell" | "commandexecution"
    ) {
        Icon::TERMINAL
    } else {
        Icon::WRENCH_AND_SCREWDRIVER
    };
    icon.render(IconSize::Small, Colors::fg_tertiary(&theme), &theme)
}

/// What the call is called, in the reader's language rather than the tool's.
fn tool_title(call: &ToolCall<'_>) -> String {
    match call.name.to_ascii_lowercase().as_str() {
        "read" => "Read".to_string(),
        "edit" => "Edited".to_string(),
        "write" => "Wrote".to_string(),
        "grep" => "Searched".to_string(),
        "glob" => "Matched files".to_string(),
        "bash" | "command" | "shell" | "commandexecution" => "Ran command".to_string(),
        _ if call.name == ORPHAN_TOOL => {
            if call.is_error {
                "Tool failed".to_string()
            } else {
                "Tool result".to_string()
            }
        }
        _ => call.name.to_string(),
    }
}

/// The one line that says what the call was pointed at.
fn tool_detail(call: &ToolCall<'_>) -> Option<String> {
    let argument = json_string(call.input, "file_path")
        .or_else(|| json_string(call.input, "pattern"))
        .or_else(|| json_string(call.input, "command"))?;
    let summarized = summarize_line(&argument, SUMMARY_CHARACTER_LIMIT);
    (!summarized.is_empty()).then_some(summarized)
}

fn group_title(group: &ActivityGroup<'_>) -> String {
    if group.is_active {
        return "Working…".to_string();
    }
    let mut title = match group.summary.seconds {
        Some(seconds) => format!("Worked for {}", duration_label(seconds)),
        None => "Worked".to_string(),
    };
    if group.summary.calls > 0 {
        title.push_str(" · ");
        title.push_str(&action_label(group.summary.calls));
        if group.summary.edits > 0 {
            title.push_str(" · ");
            title.push_str(&edit_label(group.summary.edits));
        }
    }
    title
}

fn action_label(count: usize) -> String {
    match count {
        1 => "1 action".to_string(),
        other => format!("{other} actions"),
    }
}

fn edit_label(count: usize) -> String {
    match count {
        1 => "1 edit".to_string(),
        other => format!("{other} edits"),
    }
}

fn failure_label(count: usize) -> String {
    match count {
        1 => "1 failed".to_string(),
        other => format!("{other} failed"),
    }
}

/// Shorten a duration to the largest unit that still says something.
fn duration_label(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        let minutes = seconds / 60;
        let remainder = seconds % 60;
        return if remainder == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {remainder}s")
        };
    }
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {minutes}m")
    }
}

// ── Detail blocks ──────────────────────────────────────────────────────────

/// What a monospaced panel is showing, which decides its ink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockTone {
    /// A code block, a tool's arguments, a tool's output.
    Standard,
    /// A tool that failed. The whole panel says so, not just its title, because
    /// the title is the first thing truncated on a narrow window.
    Failure,
}

/// A labelled block of monospaced text on a bordered panel.
fn mono_panel(label: &str, body: &str, tone: BlockTone) -> AnyElement {
    let theme = global::current();
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .rounded(px_len(Radii::MD))
        .overflow_hidden()
        .border(px_len(BorderWidth::HAIRLINE))
        .border_color(Colors::border_subtle(&theme))
        .bg(Colors::bg_surface(&theme))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .px(px_len(Spacing::MD))
                .py(px_len(Spacing::XS))
                .bg(Colors::bg_elevated(&theme))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .fx_caption()
                        .truncate()
                        .text_color(match tone {
                            BlockTone::Standard => Colors::fg_tertiary(&theme),
                            BlockTone::Failure => Colors::error(&theme),
                        })
                        .child(SharedString::from(label.to_string())),
                ),
        )
        .child(
            div()
                .min_w_0()
                .px(px_len(Spacing::MD))
                .py(px_len(Spacing::MD))
                .fx_role(TextRole::MonoSmall)
                .text_color(match tone {
                    BlockTone::Standard => Colors::fg_secondary(&theme),
                    BlockTone::Failure => Colors::error(&theme),
                })
                .child(SharedString::from(bounded_detail(body))),
        )
        .into_any_element()
}

/// Cap a detail block. A tool can return megabytes, and a transcript that grows
/// without bound is a layout stall as well as a memory one.
fn bounded_detail(text: &str) -> String {
    if text.chars().count() <= DETAIL_CHARACTER_LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(DETAIL_CHARACTER_LIMIT).collect();
    format!("{head}\n… truncated")
}

/// The first line of a value, elided to fit a row.
fn summarize_line(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= limit {
        return line.to_string();
    }
    let head: String = line.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

/// A string field out of a tool's JSON arguments, when they parse and have one.
///
/// Tool arguments come from a provider's JSON, so this can fail for a dozen
/// reasons. Falling back to the raw text is the point: the row still has to say
/// something about what it ran.
fn json_string(input: &str, key: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(input).ok()?;
    value.get(key)?.as_str().map(str::to_string)
}

// ── The question exchange ──────────────────────────────────────────────────

/// A question the model put to the reader, lifted out of the tool traffic.
///
/// The grouping policy keeps `AskUserQuestion` out of the activity groups, so
/// this is where it lands: a card rather than a row, because the question is the
/// point of it and the raw arguments are not.
fn question_card(input: &str) -> AnyElement {
    let theme = global::current();
    let questions = parse_questions(input);

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .fx_body_medium()
                .text_color(Colors::fg(&theme))
                .child(SharedString::from(if questions.is_empty() {
                    QUESTION_TOOL_NAME.to_string()
                } else {
                    "Asked you".to_string()
                })),
        )
        .child(if questions.is_empty() {
            div().w(px(0.0)).h(px(0.0)).into_any_element()
        } else {
            badge(SharedString::from("Transcript"), BadgeTone::Neutral, false).into_any_element()
        });

    let mut body = div()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::MD))
        .p(px_len(Spacing::MD))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(Icon::QUESTIONMARK_BUBBLE_FILL.render(
                    IconSize::Medium,
                    Colors::accent_secondary(&theme),
                    &theme,
                ))
                .child(header),
        );

    if questions.is_empty() {
        // Not a shape this renderer recognises. Showing the arguments is honest:
        // the call happened, and the reader sees that it did.
        return div()
            .flex()
            .flex_col()
            .gap(px_len(Spacing::XXS))
            .child(body)
            .child(mono_panel("Arguments", input, BlockTone::Standard))
            .into_any_element();
    }

    for (index, question) in questions.iter().enumerate() {
        if index > 0 {
            body = body.child(divider_with_axis(DividerAxis::Horizontal));
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::SM))
                .child(
                    div()
                        .fx_overline()
                        .text_color(Colors::accent_secondary(&theme))
                        .truncate()
                        .child(SharedString::from(question.header.clone())),
                )
                .child(
                    div()
                        .fx_body_medium()
                        .text_color(Colors::fg(&theme))
                        .child(SharedString::from(question.question.clone())),
                ),
        );
        for (option_index, option) in question.options.iter().enumerate() {
            body = body.child(question_option(option_index, option));
        }
    }

    panel(body).into_any_element()
}

fn question_option(index: usize, option: &QuestionOption) -> AnyElement {
    let theme = global::current();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px_len(Spacing::SM))
        .child(
            div()
                .w(px(OPTION_MARKER_WIDTH))
                .flex_shrink_0()
                .fx_role(TextRole::MonoSmall)
                .text_color(Colors::fg_tertiary(&theme))
                .child(SharedString::from(format!("{}.", index + 1))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px_len(Spacing::XXXS))
                .child(
                    div()
                        .fx_body()
                        .text_color(Colors::fg(&theme))
                        .child(SharedString::from(option.label.clone())),
                )
                .child(if option.description.is_empty() {
                    div().w(px(0.0)).h(px(0.0)).into_any_element()
                } else {
                    div()
                        .fx_caption()
                        .text_color(Colors::fg_secondary(&theme))
                        .child(SharedString::from(option.description.clone()))
                        .into_any_element()
                }),
        )
        .into_any_element()
}

/// One question out of a recorded `AskUserQuestion` call.
struct RecordedQuestion {
    header: String,
    question: String,
    options: Vec<QuestionOption>,
}

/// Read the questions out of a recorded call's arguments.
///
/// Anything unrecognised yields nothing rather than a guess, and the caller falls
/// back to showing the raw call.
fn parse_questions(input: &str) -> Vec<RecordedQuestion> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Vec::new();
    };
    let Some(raw) = value.get("questions").and_then(|list| list.as_array()) else {
        return Vec::new();
    };
    raw.iter()
        .filter_map(|entry| {
            let question = entry.get("question")?.as_str()?.trim();
            if question.is_empty() {
                return None;
            }
            let header = entry
                .get("header")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Question");
            let options = entry
                .get("options")
                .and_then(|list| list.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|option| {
                            let label = option.get("label")?.as_str()?.trim();
                            if label.is_empty() {
                                return None;
                            }
                            Some(QuestionOption {
                                label: label.to_string(),
                                description: option
                                    .get("description")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(RecordedQuestion {
                header: header.to_string(),
                question: question.to_string(),
                options,
            })
        })
        .collect()
}

// ── What the provider is blocked on ────────────────────────────────────────

/// The tray that sits below the transcript while the turn needs the reader.
///
/// Not a transcript row. An approval and a question are addressed to the person
/// looking at the window, and they have to be reachable without scrolling back
/// through the turn that raised them.
fn pending_prompts(conversation: &crate::Conversation) -> AnyElement {
    let approvals = conversation.pending_approvals();
    let questions = conversation.pending_questions();
    if approvals.is_empty() && questions.is_empty() {
        return div().w(px(0.0)).h(px(0.0)).into_any_element();
    }
    let theme = global::current();

    let mut cards = div().flex().flex_col().gap(px_len(Spacing::MD));
    if !approvals.is_empty() {
        cards = cards.child(tray_title(
            Icon::HAND_RAISED_FILL,
            "approval required",
            approvals.len(),
        ));
        for approval in approvals {
            cards = cards.child(approval_card(approval));
        }
    }
    if !questions.is_empty() {
        cards = cards.child(tray_title(
            Icon::QUESTIONMARK_BUBBLE_FILL,
            "question",
            questions.len(),
        ));
        for question in questions {
            cards = cards.child(pending_question_card(question));
        }
    }
    cards = cards.child(awaiting_response_note());

    div()
        .flex_shrink_0()
        .w_full()
        .flex()
        .flex_col()
        // Centred on the same measure as the transcript above it, so a card in
        // the tray lines up with the rows it belongs to.
        .items_center()
        .bg(Colors::content_bg(&theme))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .max_w(px(Layout::READABLE_CONTENT_WIDTH))
                .px(px_len(Spacing::XXL))
                .pt(px_len(Spacing::SM))
                .pb(px_len(Spacing::MD))
                .child(cards),
        )
        .into_any_element()
}

fn tray_title(icon: Icon, label: &str, count: usize) -> AnyElement {
    let theme = global::current();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .child(icon.render(IconSize::Small, Colors::fg_secondary(&theme), &theme))
        .child(
            div()
                .fx_role(TextRole::CaptionMedium)
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from(match count {
                    1 => label.to_string(),
                    other => format!("{other} {label}"),
                })),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .border_b_1()
                .border_color(Colors::border_subtle(&theme)),
        )
        .into_any_element()
}

fn approval_card(approval: &PendingApproval) -> AnyElement {
    let theme = global::current();
    let tone = match approval.risk {
        RiskLevelSerde::Safe => BadgeTone::Success,
        RiskLevelSerde::Moderate => BadgeTone::Warning,
        RiskLevelSerde::Dangerous => BadgeTone::Error,
    };

    div()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .p(px_len(Spacing::LG))
        .rounded(px_len(Radii::XL))
        // Warning-muted rather than a plain surface: the turn is stopped on this,
        // and a tray that looks like the rest of the surface does not say so.
        .bg(Colors::warning_muted(&theme))
        .border_1()
        .border_color(Colors::warning(&theme).with_alpha(0.18))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px_len(Spacing::SM))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .fx_body_medium()
                        .text_color(Colors::fg(&theme))
                        .truncate()
                        .child(SharedString::from(approval.tool_name.clone())),
                )
                .child(badge(risk_label(approval.risk), tone, false)),
        )
        .child(
            div()
                .fx_caption()
                .text_color(Colors::fg_secondary(&theme))
                .child(SharedString::from(describe_arguments(&approval.input))),
        )
        .child(mono_panel(
            "Arguments",
            &pretty_arguments(&approval.input),
            BlockTone::Standard,
        ))
        .into_any_element()
}

fn risk_label(risk: RiskLevelSerde) -> SharedString {
    SharedString::from(match risk {
        RiskLevelSerde::Safe => "Safe",
        RiskLevelSerde::Moderate => "Moderate",
        RiskLevelSerde::Dangerous => "Dangerous",
    })
}

fn pending_question_card(question: &PendingQuestion) -> AnyElement {
    let theme = global::current();
    let trimmed = question.header.trim();
    let header = if trimmed.is_empty() {
        "Question".to_string()
    } else {
        trimmed.to_string()
    };
    let text = if question.question.trim().is_empty() {
        "The provider asked something without saying what.".to_string()
    } else {
        question.question.clone()
    };

    let mut body = div()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::SM))
        .p(px_len(Spacing::MD))
        .child(
            div()
                .fx_overline()
                .text_color(Colors::accent_secondary(&theme))
                .truncate()
                .child(SharedString::from(header)),
        )
        .child(
            div()
                .fx_body_medium()
                .text_color(Colors::fg(&theme))
                .child(SharedString::from(text)),
        );
    for (index, option) in question.options.iter().enumerate() {
        body = body.child(question_option(index, option));
    }

    panel(body).into_any_element()
}

/// What the approval is about, in one line.
fn describe_arguments(input: &serde_json::Value) -> String {
    let command = input.get("command").and_then(|value| value.as_str());
    let file = input.get("file_path").and_then(|value| value.as_str());
    match (command, file) {
        (Some(command), _) => format!("Run: {}", summarize_line(command, SUMMARY_CHARACTER_LIMIT)),
        (None, Some(file)) => format!("Change: {file}"),
        (None, None) => "This tool call needs your approval.".to_string(),
    }
}

/// The arguments, pretty-printed when they are JSON and verbatim when they are not.
fn pretty_arguments(input: &serde_json::Value) -> String {
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// Direct the reader to the dialog that owns the provider response.
fn awaiting_response_note() -> AnyElement {
    let theme = global::current();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px_len(Spacing::SM))
        .fx_caption()
        .text_color(Colors::fg_tertiary(&theme))
        .child(Icon::HOURGLASS_BOTTOMHALF_FILLED.render(
            IconSize::Micro,
            Colors::fg_tertiary(&theme),
            &theme,
        ))
        .child(div().flex_1().min_w_0().child(SharedString::from(
            "Waiting for your response in the dialog above.",
        )))
        .into_any_element()
}

// ── Motion ─────────────────────────────────────────────────────────────────

/// How far the spinner has turned, in degrees.
fn spin_degrees(frame: u32) -> f32 {
    // Twelve steps over a full turn, so the arc lands where it started.
    (frame % 12) as f32 * 30.0
}

/// Which dot of the typing indicator is lit.
fn typing_phase(frame: u32) -> usize {
    ((frame / 8) % 3) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_agent::conversation::{Conversation, MAX_RETAINED_MESSAGES};
    use fx_agent::provider::{Lifecycle, ProviderEvent};
    use gpui::{Bounds, Context, Pixels, Render, TestAppContext, VisualTestContext, size};
    use std::ops::Deref;

    // ── Pure logic ──────────────────────────────────────────────────────────

    fn message(role: MessageRole, content: Vec<MessageContent>) -> ConversationMessage {
        ConversationMessage::new(role, content)
    }

    fn tool_use(id: &str, name: &str, input: &str) -> ConversationMessage {
        message(
            MessageRole::Assistant,
            vec![MessageContent::ToolUse {
                id: id.into(),
                name: name.into(),
                input: input.into(),
            }],
        )
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> ConversationMessage {
        message(
            MessageRole::Tool,
            vec![MessageContent::ToolResult {
                id: id.into(),
                content: content.into(),
                is_error,
            }],
        )
    }

    fn text(role: MessageRole, body: &str) -> ConversationMessage {
        ConversationMessage::text(role, body)
    }

    /// Borrow an owned transcript the way the engine lends it out.
    /// The thread every grouping test pretends to be rendering.
    const CONVERSATION: Uuid = Uuid::nil();

    fn borrowed(source: &[ConversationMessage]) -> Vec<&ConversationMessage> {
        source.iter().collect()
    }

    /// One activity group out of a built transcript.
    fn only_group<'a>(rows: &'a [Row<'a>]) -> &'a ActivityGroup<'a> {
        let mut groups = rows.iter().filter_map(|row| match row {
            Row::Activity(group) => Some(group),
            Row::Message(_) => None,
        });
        let first = groups.next().expect("no activity group");
        assert!(groups.next().is_none(), "more than one activity group");
        first
    }

    #[test]
    fn the_readers_own_prompt_is_never_folded_into_an_activity_group() {
        let source = [
            text(MessageRole::User, "read this"),
            tool_use("t1", "Read", "{}"),
            tool_result("t1", "source", false),
            text(MessageRole::Assistant, "found it"),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);

        let kinds: Vec<bool> = rows
            .iter()
            .map(|row| matches!(row, Row::Activity(_)))
            .collect();
        assert_eq!(
            kinds,
            vec![false, true, false],
            "the prompt, the group, and the prose that followed it"
        );
    }

    #[test]
    fn prose_around_a_call_stays_outside_the_group() {
        // A tool call splits the turn, so the two sentences either side of it
        // are separate rows rather than one row with the call buried in it.
        let source = [
            text(MessageRole::Assistant, "let me look"),
            tool_use("t1", "Read", "{}"),
            text(MessageRole::Assistant, "found it"),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[1], Row::Activity(_)));
    }

    #[test]
    fn only_the_trailing_group_of_a_running_turn_is_live() {
        // An earlier group in the same turn has already been overtaken, and
        // holding it open would leave a spinner running on settled work.
        let source = [
            tool_use("t1", "Read", "{}"),
            text(MessageRole::Assistant, "and now"),
            tool_use("t2", "Grep", "{}"),
        ];
        let rows = build_rows(&borrowed(&source), true, CONVERSATION);
        let groups: Vec<&ActivityGroup<'_>> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Activity(group) => Some(group),
                Row::Message(_) => None,
            })
            .collect();
        assert_eq!(groups.len(), 2);
        assert!(!groups[0].is_active, "the settled group was left open");
        assert!(groups[1].is_active, "the live group was not opened");
    }

    #[test]
    fn a_settled_turn_has_no_live_group() {
        let source = [
            tool_use("t1", "Read", "{}"),
            tool_result("t1", "out", false),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        assert!(!only_group(&rows).is_active);
    }

    #[test]
    fn two_groups_in_one_turn_get_different_keys() {
        // The key is what an expansion is stored against; a collision would open
        // both groups with one click.
        let source = [
            tool_use("t1", "Read", "{}"),
            text(MessageRole::Assistant, "meanwhile"),
            tool_use("t2", "Grep", "{}"),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        let keys: Vec<&String> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Activity(group) => Some(&group.key),
                Row::Message(_) => None,
            })
            .collect();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0], keys[1]);
    }

    #[test]
    fn a_result_is_paired_with_the_call_it_belongs_to() {
        let source = [
            tool_use("read", "Read", "{\"file_path\":\"/a/b.rs\"}"),
            tool_use("grep", "Grep", "{\"pattern\":\"fn main\"}"),
            tool_result("grep", "3 matches", false),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        let group = only_group(&rows);
        assert_eq!(group.calls.len(), 2);
        let grep = group.calls.iter().find(|call| call.id == "grep").unwrap();
        assert_eq!(grep.output, Some("3 matches"));
        assert!(grep.result_recorded);
        let read = group.calls.iter().find(|call| call.id == "read").unwrap();
        assert!(
            read.output.is_none(),
            "one result was attached to the wrong call"
        );
        assert!(!read.result_recorded);
    }

    #[test]
    fn a_result_with_no_call_still_gets_a_row() {
        // A resumed provider can replay a result, and a restored transcript can
        // arrive with its calls trimmed. A failure with nothing above it is
        // exactly what the reader needs to see.
        let source = [tool_result("orphan", "permission denied", true)];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        let group = only_group(&rows);
        assert_eq!(group.calls.len(), 1);
        assert_eq!(group.calls[0].name, ORPHAN_TOOL);
        assert!(group.calls[0].is_error);
        assert_eq!(group.summary.failures, 1);
    }

    #[test]
    fn two_calls_with_no_id_do_not_collide() {
        let source = [
            tool_use("", "Read", "{}"),
            tool_use("", "Read", "{}"),
            tool_result("", "done", false),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        let group = only_group(&rows);
        assert_eq!(group.calls.len(), 3, "an empty id matched the wrong call");
    }

    #[test]
    fn the_header_counts_calls_edits_and_failures() {
        let source = [
            tool_use("a", "Read", "{}"),
            tool_use("b", "Edit", "{}"),
            tool_use("c", "Write", "{}"),
            tool_use("d", "Bash", "{}"),
            tool_result("b", "denied", true),
        ];
        let rows = build_rows(&borrowed(&source), false, CONVERSATION);
        let group = only_group(&rows);
        assert_eq!(group.summary.calls, 4);
        assert_eq!(group.summary.edits, 2);
        assert_eq!(group.summary.failures, 1);
        assert!(group_title(group).contains("4 actions"));
        assert!(group_title(group).contains("2 edits"));
    }

    #[test]
    fn the_question_tool_is_never_counted_as_work() {
        let source = [tool_use("q", QUESTION_TOOL_NAME, "{\"questions\":[]}")];
        let summary = summarize(&[&source[0]]);
        assert_eq!(summary.calls, 0);
    }

    #[test]
    fn a_group_with_no_timestamp_spread_reports_no_duration() {
        // A burst inside one instant says nothing useful beside the call count.
        let source = [tool_use("a", "Read", "{}")];
        assert_eq!(summarize(&[&source[0]]).seconds, None);
    }

    #[test]
    fn a_working_group_is_titled_as_working() {
        let source = [tool_use("a", "Read", "{}")];
        let rows = build_rows(&borrowed(&source), true, CONVERSATION);
        assert_eq!(group_title(only_group(&rows)), "Working…");
    }

    #[test]
    fn a_tool_title_says_what_the_reader_did() {
        let cases = [
            ("Read", "Read"),
            ("Edit", "Edited"),
            ("Write", "Wrote"),
            ("Grep", "Searched"),
            ("Glob", "Matched files"),
            ("Bash", "Ran command"),
            ("Frobnicate", "Frobnicate"),
        ];
        for (name, expected) in cases {
            let call = ToolCall {
                id: "t".into(),
                name,
                input: "",
                output: None,
                is_error: false,
                result_recorded: false,
            };
            assert_eq!(tool_title(&call), expected, "for {name}");
        }
    }

    #[test]
    fn a_tool_detail_names_what_the_call_was_pointed_at() {
        let cases = [
            (
                "Read",
                "{\"file_path\":\"/a/b.rs\"}",
                Some("/a/b.rs".to_string()),
            ),
            (
                "Grep",
                "{\"pattern\":\"fn main\"}",
                Some("fn main".to_string()),
            ),
            (
                "Bash",
                "{\"command\":\"ls -la\"}",
                Some("ls -la".to_string()),
            ),
            // Not JSON: the row still has to say something.
            ("Custom", "not json at all", None),
            ("Read", "{}", None),
        ];
        for (name, input, expected) in cases {
            let call = ToolCall {
                id: "t".into(),
                name,
                input,
                output: None,
                is_error: false,
                result_recorded: false,
            };
            assert_eq!(tool_detail(&call), expected, "for {name} {input}");
        }
    }

    #[test]
    fn durations_use_the_largest_unit_that_says_something() {
        assert_eq!(duration_label(1), "1s");
        assert_eq!(duration_label(59), "59s");
        assert_eq!(duration_label(60), "1m");
        assert_eq!(duration_label(90), "1m 30s");
        assert_eq!(duration_label(3_600), "1h");
        assert_eq!(duration_label(3_660), "1h 1m");
    }

    #[test]
    fn a_detail_block_is_capped() {
        let huge = "x".repeat(DETAIL_CHARACTER_LIMIT * 2);
        let bounded = bounded_detail(&huge);
        assert!(bounded.ends_with("… truncated"));
        assert_eq!(
            bounded.chars().filter(|c| *c == 'x').count(),
            DETAIL_CHARACTER_LIMIT
        );
        assert_eq!(bounded_detail("short"), "short");
    }

    #[test]
    fn a_summary_takes_the_first_line_and_elides_the_rest() {
        assert_eq!(summarize_line("  first  \nsecond", 100), "first");
        let long = "a".repeat(SUMMARY_CHARACTER_LIMIT + 10);
        let summarized = summarize_line(&long, SUMMARY_CHARACTER_LIMIT);
        assert_eq!(summarized.chars().count(), SUMMARY_CHARACTER_LIMIT);
        assert!(summarized.ends_with('…'));
    }

    #[test]
    fn a_recorded_question_is_read_out_of_its_arguments() {
        let input = r#"{"questions":[{"header":"Scope","question":"Which files?",
            "options":[{"label":"All","description":"everywhere"},
                       {"label":"Tests"}]}]}"#;
        let parsed = parse_questions(input);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].header, "Scope");
        assert_eq!(parsed[0].question, "Which files?");
        assert_eq!(parsed[0].options.len(), 2);
        assert_eq!(parsed[0].options[0].description, "everywhere");
        assert_eq!(parsed[0].options[1].description, "");
    }

    #[test]
    fn an_unrecognised_question_call_reads_as_nothing_rather_than_as_a_guess() {
        // The caller falls back to showing the raw call, so returning something
        // invented here would be worse than returning nothing.
        for input in ["", "not json", "{}", r#"{"questions":"wrong shape"}"#] {
            assert!(parse_questions(input).is_empty(), "for {input:?}");
        }
    }

    #[test]
    fn a_question_with_no_header_falls_back_to_a_label() {
        let parsed = parse_questions(r#"{"questions":[{"question":"And?"}]}"#);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].header, "Question");
    }

    #[test]
    fn an_empty_question_is_dropped() {
        let parsed = parse_questions(r#"{"questions":[{"question":"   "}]}"#);
        assert!(parsed.is_empty());
    }

    #[test]
    fn an_empty_transcript_produces_no_rows() {
        assert!(build_rows(&[], false, CONVERSATION).is_empty());
    }

    #[test]
    fn a_disclosure_opens_closes_and_is_forgotten_with_its_group() {
        // A thread that ran for hours would otherwise accumulate one map entry
        // per activity group it ever showed.
        let conversation = Uuid::from_u128(0xfeed);
        let live = group_key(conversation, 0);
        let stale = group_key(conversation, 1);
        // A different thread, so its group has a key of its own.
        let other = group_key(Uuid::from_u128(0xbeef), 0);

        assert!(!is_disclosed(&live), "starts closed");
        toggle_disclosure(&live);
        assert!(is_disclosed(&live));
        toggle_disclosure(&live);
        assert!(!is_disclosed(&live));

        toggle_disclosure(&live);
        toggle_disclosure(&stale);
        toggle_disclosure(&other);

        prune_disclosures(conversation, 1, 0, &HashSet::from([live.clone()]));
        assert!(!is_disclosed(&stale), "a group that went away was kept");
        assert!(is_disclosed(&live), "a live group was pruned");
        assert!(is_disclosed(&other), "another thread's group was pruned");
    }

    #[test]
    fn pruning_runs_once_per_content_revision() {
        // A stream produces a revision on every delta, and pruning is a map
        // rebuild, so running it unconditionally would cost more than it saves.
        let conversation = Uuid::from_u128(0xbeef);
        let stale = group_key(conversation, 1);
        prune_disclosures(conversation, 7, 0, &HashSet::new());
        toggle_disclosure(&stale);
        // Same revision: the pass is skipped, so the entry survives.
        prune_disclosures(conversation, 7, 0, &HashSet::new());
        assert!(is_disclosed(&stale));
        // Next revision: the group really is gone.
        prune_disclosures(conversation, 8, 0, &HashSet::new());
        assert!(!is_disclosed(&stale));
    }

    /// The key a group at `index` of a turn would be stored under.
    fn group_key(conversation: Uuid, index: usize) -> String {
        let first = ConversationMessage::text(MessageRole::Assistant, "x");
        activity_key(conversation, &[&first], index)
    }

    #[test]
    fn one_scroll_handle_survives_across_passes_for_a_thread() {
        // Two passes must land on the same handle, or the reader's offset is
        // thrown away on every repaint.
        let conversation = Uuid::from_u128(0xc0de);
        let first = scroll_handle(conversation);
        let second = scroll_handle(conversation);
        first.set_offset(gpui::point(px(0.0), px(-42.0)));
        assert_eq!(
            f64::from(second.offset().y),
            -42.0,
            "two passes must land on the same handle"
        );
        assert_eq!(
            f64::from(scroll_handle(Uuid::from_u128(0xc0df)).offset().y),
            0.0,
            "another thread got its own"
        );
    }

    // ── Rendering ───────────────────────────────────────────────────────────

    /// The smallest thing that can call `view` with a real window.
    struct Harness {
        conversation: Conversation,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(view(&self.conversation, window, cx))
        }
    }

    /// Paint the transcript once and insist it survives.
    ///
    /// This is the only check that reaches every branch of the element tree, and
    /// a transcript that panics is not a degraded view — it is a window that
    /// will not open.
    fn paint(conversation: Conversation) {
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_window, _cx| Harness { conversation });
        test.refresh().expect("the transcript painted");
        window
            .update(&mut test, |_window, _entity, _cx| {})
            .expect("the window is still alive");
    }

    fn conversation_with(events: Vec<ProviderEvent>) -> Conversation {
        let mut conversation = Conversation::new(Uuid::nil());
        for event in events {
            conversation.apply(event);
        }
        conversation
    }

    #[test]
    fn an_empty_conversation_paints() {
        paint(Conversation::new(Uuid::nil()));
    }

    fn measured_transcript(
        conversation: Conversation,
        width: f32,
        height: f32,
        selectors: &[&'static str],
    ) -> Vec<Bounds<Pixels>> {
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_window, _cx| Harness { conversation });
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(width), px(height)));
        test.refresh()
            .expect("the transcript painted at its new size");
        selectors
            .iter()
            .map(|selector| visual.debug_bounds(selector).expect("measured element"))
            .collect()
    }

    #[test]
    fn the_empty_state_is_centered_in_the_available_viewport() {
        for (width, height) in [(460.0, 480.0), (900.0, 800.0)] {
            let measured = measured_transcript(
                Conversation::new(Uuid::new_v4()),
                width,
                height,
                &[SCROLL_ID, EMPTY_SELECTOR],
            );
            let (viewport, notice) = (measured[0], measured[1]);
            assert_eq!(viewport.size, size(px(width), px(height)));
            assert!(
                (notice.center().y - viewport.center().y).abs() <= px(1.0),
                "empty state is not vertically centered: {notice:?} in {viewport:?}"
            );
            assert!(
                (notice.center().x - viewport.center().x).abs() <= px(1.0),
                "empty state is not horizontally centered: {notice:?} in {viewport:?}"
            );
        }
    }

    #[test]
    fn messages_keep_their_reading_measure_and_fit_narrow_columns() {
        for width in [320.0, Layout::MINIMUM_CONVERSATION_WIDTH, 1_100.0] {
            let mut conversation = Conversation::new(Uuid::new_v4());
            conversation.append(text(
                MessageRole::User,
                &"A prompt with enough words to wrap on a narrow window. ".repeat(8),
            ));
            conversation.append(text(
                MessageRole::Assistant,
                &"An answer with enough words to wrap and remain comfortably readable. ".repeat(8),
            ));
            let measured = measured_transcript(
                conversation,
                width,
                1_100.0,
                &[COLUMN_SELECTOR, USER_SELECTOR, ASSISTANT_SELECTOR],
            );
            let (column, user, assistant) = (measured[0], measured[1], measured[2]);
            let inset = px(Spacing::XXL);
            assert_eq!(
                column.size.width,
                px(width.min(Layout::READABLE_CONTENT_WIDTH))
            );
            assert_eq!(assistant.origin.x, column.origin.x + inset);
            assert_eq!(assistant.right(), column.right() - inset);
            assert!(
                user.origin.x >= assistant.origin.x,
                "user bubble escaped at {width}: {user:?}"
            );
            assert_eq!(user.right(), assistant.right());
            assert!(user.size.width <= px(Layout::USER_MESSAGE_MAX_WIDTH));
            assert!(assistant.origin.y >= user.bottom() + px(Spacing::XXL));
        }
    }

    #[test]
    fn a_populated_transcript_grows_and_scrolls_instead_of_compressing_its_rows() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let id = conversation.id;
        for index in 0..30 {
            conversation.append(text(
                MessageRole::Assistant,
                &format!("Message {index} stays at its natural reading height."),
            ));
        }
        let measured = measured_transcript(
            conversation,
            Layout::MINIMUM_CONVERSATION_WIDTH,
            400.0,
            &[SCROLL_ID, COLUMN_SELECTOR],
        );
        let (viewport, column) = (measured[0], measured[1]);
        let handle = scroll_handle(id);
        assert_eq!(viewport.size.height, px(400.0));
        assert!(column.size.height > viewport.size.height * 2.0);
        assert!(handle.max_offset().height > viewport.size.height);
        assert!(
            (handle.offset().y + handle.max_offset().height).abs() <= px(1.0),
            "a newly opened transcript should remain pinned to its newest content"
        );
    }

    #[test]
    fn a_whole_turn_paints_with_every_kind_of_row() {
        let conversation = conversation_with(vec![
            ProviderEvent::TextDelta("I'll look at that.".into()),
            ProviderEvent::ToolUse {
                id: "t1".into(),
                name: "Read".into(),
                input: r#"{"file_path":"/tmp/x.rs"}"#.into(),
            },
            ProviderEvent::ToolResult {
                id: "t1".into(),
                content: "file contents".into(),
                is_error: false,
            },
            ProviderEvent::ToolUse {
                id: "t2".into(),
                name: "Edit".into(),
                input: r#"{"file_path":"/tmp/x.rs"}"#.into(),
            },
            ProviderEvent::ToolResult {
                id: "t2".into(),
                content: "permission denied".into(),
                is_error: true,
            },
            ProviderEvent::TextDelta("I could not write it.".into()),
            ProviderEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]);
        paint(conversation);
    }

    #[test]
    fn a_message_with_no_usable_content_paints() {
        // The restore path can produce one. It must still get a row rather than
        // taking the window down.
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.append(ConversationMessage::new(MessageRole::Assistant, vec![]));
        conversation.append(message(
            MessageRole::Assistant,
            vec![MessageContent::Text { text: "   ".into() }],
        ));
        paint(conversation);
    }

    #[test]
    fn images_paint_as_a_notice_rather_than_an_empty_box() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.append(message(
            MessageRole::User,
            vec![
                MessageContent::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
                MessageContent::ImageAsset(ConversationImageAssetReference {
                    project_id: Uuid::nil(),
                    agent_id: Uuid::nil(),
                    message_id: Uuid::nil(),
                    content_index: 0,
                    mime_type: "image/jpeg".into(),
                    byte_count: 42,
                }),
            ],
        ));
        paint(conversation);
    }

    #[test]
    fn code_and_a_question_call_paint() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.append(message(
            MessageRole::Assistant,
            vec![
                MessageContent::Code {
                    language: "rust".into(),
                    code: "fn main() {}".into(),
                },
                MessageContent::Code {
                    language: "  ".into(),
                    code: "unlabelled".into(),
                },
                MessageContent::ToolUse {
                    id: "q".into(),
                    name: QUESTION_TOOL_NAME.into(),
                    input: r#"{"questions":[{"header":"Scope","question":"Which files?","options":[{"label":"All"}]}]}"#.into(),
                },
                // An argument shape this renderer does not recognise.
                MessageContent::ToolUse {
                    id: "q2".into(),
                    name: QUESTION_TOOL_NAME.into(),
                    input: "not json".into(),
                },
                MessageContent::ToolUse {
                    id: "t9".into(),
                    name: String::new(),
                    input: String::new(),
                },
            ],
        ));
        paint(conversation);
    }

    #[test]
    fn a_failed_turn_paints_its_banner() {
        let conversation = conversation_with(vec![ProviderEvent::Error("rate limited".into())]);
        paint(conversation);
    }

    #[test]
    fn a_live_stream_paints() {
        let conversation = conversation_with(vec![
            ProviderEvent::Lifecycle(Lifecycle::TurnStarted),
            ProviderEvent::TextDelta("Let me check.".into()),
            ProviderEvent::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: r#"{"command":"ls"}"#.into(),
            },
            ProviderEvent::ToolResult {
                id: "t1".into(),
                content: "a.rs".into(),
                is_error: false,
            },
            ProviderEvent::TextDelta("Here ".into()),
            ProviderEvent::TextDelta("is the answer.".into()),
        ]);
        assert!(conversation.is_streaming(), "nothing streamed");
        assert_eq!(conversation.streaming_text(), "Here is the answer.");
        paint(conversation);
    }

    #[test]
    fn the_live_tail_is_dropped_when_the_turn_ends() {
        // The commit has to happen before the row can be rebuilt, or the last
        // sentence of a turn never appears at all.
        let conversation = conversation_with(vec![
            ProviderEvent::Lifecycle(Lifecycle::TurnStarted),
            ProviderEvent::TextDelta("The answer.".into()),
            ProviderEvent::Done {
                stop_reason: "end_turn".into(),
            },
        ]);
        assert!(!conversation.is_streaming());
        assert_eq!(conversation.message_count(), 1);
        paint(conversation);
    }

    #[test]
    fn a_running_turn_with_nothing_streamed_paints_its_indicator() {
        let conversation = conversation_with(vec![ProviderEvent::Lifecycle(Lifecycle::Preparing)]);
        assert!(conversation.is_busy());
        assert!(!conversation.is_streaming());
        paint(conversation);
    }

    #[test]
    fn a_turn_waiting_on_the_reader_paints_the_tray() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: r#"{"command":"rm -rf /"}"#.into(),
        });
        conversation.add_approval(PendingApproval {
            id: "a1".into(),
            tool_name: "Bash".into(),
            input: serde_json::json!({ "command": "rm -rf /" }),
            risk: RiskLevelSerde::Dangerous,
        });
        conversation.add_question(PendingQuestion {
            id: "q1".into(),
            question: "Which database?".into(),
            header: "Scope".into(),
            options: vec![QuestionOption {
                label: "Postgres".into(),
                description: String::new(),
            }],
            is_other: true,
        });
        assert!(conversation.is_waiting_for_user());
        paint(conversation);
    }

    #[test]
    fn an_approval_with_no_arguments_still_renders() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.add_approval(PendingApproval {
            id: "a1".into(),
            tool_name: "Custom".into(),
            input: serde_json::Value::Null,
            risk: RiskLevelSerde::Safe,
        });
        paint(conversation);
    }

    #[test]
    fn a_question_with_no_text_still_renders() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.add_question(PendingQuestion {
            id: "q1".into(),
            question: "  ".into(),
            header: String::new(),
            options: Vec::new(),
            is_other: false,
        });
        paint(conversation);
    }

    #[test]
    fn a_long_transcript_paints() {
        // The retention cap is 250, and the viewport is one scrollable element
        // over all of it. This is the shape that stalls a layout if anything can.
        let mut conversation = Conversation::new(Uuid::nil());
        for index in 0..MAX_RETAINED_MESSAGES {
            if index % 3 == 0 {
                conversation.apply(ProviderEvent::ToolUse {
                    id: format!("t{index}"),
                    name: "Read".into(),
                    input: format!("{{\"file_path\":\"/tmp/{index}.rs\"}}"),
                });
                conversation.apply(ProviderEvent::ToolResult {
                    id: format!("t{index}"),
                    content: format!("contents of {index}"),
                    is_error: index % 9 == 0,
                });
            } else {
                conversation.append(text(MessageRole::Assistant, &format!("line {index}")));
            }
        }
        assert_eq!(conversation.message_count(), MAX_RETAINED_MESSAGES);
        paint(conversation);
    }
}
