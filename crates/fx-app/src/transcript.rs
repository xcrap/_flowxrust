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
//! not a `Render` entity, so it owns no per-thread state of its own. The list
//! state and the disclosure flags live in a thread-local — the same mechanism
//! the theme uses, because GPUI renders on one thread. Neither is `Send`, which
//! is why they cannot become a `gpui::Global` either. When `AppState` grows the
//! per-thread UI state the shell already owns, these move there. Until then
//! the shell tells this file when a thread is deleted, and every pass sweeps
//! out memory for threads the state no longer holds.
//!
//! A second constraint is the conversation's lock. A running turn takes it for
//! every streamed delta, so a render reads the conversation into a
//! [`Snapshot`] and lets go before a single element is built. The snapshot
//! holds the conversation's own shared messages rather than copies of them,
//! which is what keeps it cheap enough to take sixty times a second.
//!
//! A third is cost. The transcript is a `gpui::list`, which builds, lays out
//! and paints only the rows in view and remembers every other row's height.
//! A pass resolves the messages into a [`RowPlan`] per row — indices, nothing
//! built — and a row's element exists only while it is on screen. That is
//! what lets a caret blink or each frame of the sidebar's slide redraw a long
//! thread without re-wrapping all of it. Two things follow:
//!
//! - **The list must hear about every row that changed**, or an off-screen row
//!   keeps a height it no longer has. Each pass stamps the rows and splices
//!   only the stretch whose stamps moved.
//! - **What is not drawn is not selectable layout.** Selection and copying
//!   read the rows off screen from the messages, through a
//!   [`selection::Document`] that produces the same paragraphs, keys and text
//!   the rows would draw.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use crate::attachments::{ImageInput, clipboard_image, preview as image_attachment};
use crate::selection::{self, Paragraph, SelectableText};
use fx_agent::Phase;
use fx_agent::conversation::state::RiskLevelSerde;
use fx_agent::conversation::{PendingApproval, PendingQuestion, QuestionOption};
use fx_core::Timestamp;
use fx_core::policy::conversation_scroll::{ConversationScrollPolicy, ScrollMetrics};
use fx_core::policy::transcript_grouping::{QUESTION_TOOL_NAME, TranscriptActivityGroupingPolicy};
use fx_core::policy::transcript_presentation::{MentionedAttachment, user_prompt};
use fx_core::{ConversationMessage, MessageContent, MessageRole};
use fx_design::components::button::text_disclosure;
use fx_design::components::surface::banner;
use fx_design::components::{
    BadgeTone, ButtonStyle, DividerAxis, MenuItem, MenuModel, MenuSection, badge, button,
    compact_action_button, context_region, divider_with_axis, empty_state, panel, spinner_arc,
    typing_indicator,
};
use fx_design::icon::Icon;
use fx_design::styled::{FxRefinement, FxStyled, px_len};
use fx_design::theme::{
    BorderWidth, Colors, IconSize, Layout, Radii, Spacing, TextRole, TextSizePreset, Typography,
    WithAlpha, global,
};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClipboardItem, CursorStyle, FocusHandle, KeyDownEvent, ListAlignment,
    ListOffset, ListState, SharedString, StyledText, Window, div, list, px,
};
use uuid::Uuid;

/// Element id of the transcript's viewport, which owns focus and the keys.
const SCROLL_ID: &str = "transcript.scroll";
/// The reading column one row is laid out in.
const COLUMN_SELECTOR: &str = "transcript.column";
const EMPTY_SELECTOR: &str = "transcript.empty";
const USER_SELECTOR: &str = "transcript.user";
const ASSISTANT_SELECTOR: &str = "transcript.assistant";
const ACTIVITY_HEADER_SELECTOR: &str = "transcript.activity-header";
const ACTIVITY_DETAILS_SELECTOR: &str = "transcript.activity-details";
const EARLIER_ACTIONS_SELECTOR: &str = "transcript.earlier-actions";
const PROMPT_BODY_SELECTOR: &str = "transcript.prompt-body";
const PROMPT_TOGGLE_SELECTOR: &str = "transcript.prompt-toggle";
const MESSAGE_COPY_SELECTOR: &str = "transcript.copy-message";
const STREAMING_SELECTOR: &str = "transcript.streaming";
const ACTIVITY_PAGE_SIZE: usize = 20;

/// How far past the viewport, above and below, rows are measured without
/// being drawn.
///
/// About a page, so paging up lands among rows whose height is known. It is
/// also why a reader far from the bottom cannot be mistaken for one near it:
/// the rows below the viewport are measured at least this far down. Rows in it
/// are laid out only when their height is unknown, and never painted.
const OVERDRAW: f32 = 600.0;

/// Space above the first row and below the last.
const LIST_PADDING: f32 = Spacing::XXL;

/// The key every paragraph of the reply still streaming in starts with.
const STREAMING_PREFIX: &str = "streaming-markdown:";

/// The hover group of one of the reader's prompts, which reveals its copy
/// button.
const USER_ROW_GROUP: &str = "transcript.user-row";

/// Every context-menu owner in the transcript starts with this, so the shell
/// can route a choice back here without knowing what was clicked.
pub(crate) const MENU_OWNER_PREFIX: &str = "transcript:";
const COPY_SELECTION: &str = "copy-selection";
const COPY_MESSAGE: &str = "copy-message";
const COPY_IMAGE: &str = "copy-image";

/// A prompt longer than this many characters collapses behind "Show more".
const COLLAPSED_CHARACTER_LIMIT: usize = 600;
/// A prompt with more lines than this collapses too, however short they are.
const COLLAPSED_LINE_LIMIT: usize = 8;
/// How much of a collapsed prompt is shaped at all.
///
/// A pasted context dump can be hundreds of kilobytes, and laying all of it out
/// only to clip it to a few lines would be the whole cost for none of the text.
const COLLAPSED_PREVIEW_CHARACTERS: usize = 2_000;

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
    /// Everything kept about one conversation, keyed by the conversation's id.
    threads: HashMap<Uuid, ThreadMemory>,
    /// Whether each activity group is open, keyed by its stable group key.
    ///
    /// Keys carry the conversation, so pruning one thread's groups cannot close
    /// another's.
    disclosures: HashMap<String, bool>,
    /// Expanded groups reveal their newest page first, even after long runs.
    detail_limits: HashMap<String, usize>,
    /// Advances once per pass.
    ///
    /// GPUI 0.2.2 has no animation clock, so nothing could drive the spinner or
    /// the typing indicator on a timer. Counting passes instead is honest about
    /// what it is: motion that only happens while there is something to report.
    frame: u32,
}

/// What the view remembers about one conversation.
///
/// All of it in one place, so forgetting a thread is one removal rather than a
/// hunt through parallel maps — the hunt is how the maps leaked before.
struct ThreadMemory {
    /// The thread this conversation belongs to, which is how a deleted thread
    /// finds its memory.
    agent: Uuid,
    /// The rows' measured heights and the reader's place among them, so a
    /// thread keeps its position when the reader switches away and back.
    ///
    /// Aligned to the bottom: with no scroll position of its own the list
    /// shows its last rows, and stays there as they grow. That is the pinned
    /// state; a scroll anywhere else anchors the view to a row instead.
    list: ListState,
    /// Set when the reader scrolls, so the next pass can ask the scroll
    /// policy whether they stopped close enough to the bottom to follow it.
    scrolled: Rc<Cell<bool>>,
    /// What the list was last told about each row.
    measured: Measured,
    /// The rows the messages resolve to, at the revision they were built at.
    plan: Option<Plan>,
    /// The viewport's focus handle, for keyboard paging and copying.
    ///
    /// Created once. A handle made fresh each pass would orphan the focused one,
    /// and focus would be lost on every repaint.
    focus: Option<FocusHandle>,
    /// The content revisions the disclosure maps were last pruned at. Without
    /// this, a long-lived thread accumulates one entry per group it has ever
    /// shown.
    pruned_at: Option<(u64, u64)>,
    /// The blink phase the last caret frame was requested for.
    blink_requested_at: Option<u32>,
    /// The messages the last pass rendered, shared with the next one. Only the
    /// thread on screen keeps these: a handle held for a thread the reader
    /// left would keep messages alive that its conversation has since dropped.
    messages: Option<SharedMessages>,
    /// The streamed text the last pass rendered, with its revision.
    streaming: Option<(u64, Rc<str>)>,
    /// Prompts the reader expanded past "Show more". Bounded by what the reader
    /// clicked, and forgotten with the thread.
    expanded_prompts: HashSet<Uuid>,
    /// The prompt whose copy button reads "Copied" until the pointer leaves it.
    copied: Option<Uuid>,
}

impl ThreadMemory {
    fn new(agent: Uuid) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        let scrolled = Rc::new(Cell::new(false));
        // Only a flag: the list calls this with its own state borrowed, so
        // nothing here may touch the list.
        let flag = scrolled.clone();
        list.set_scroll_handler(move |_, _, _| flag.set(true));
        Self {
            agent,
            list,
            scrolled,
            measured: Measured::default(),
            plan: None,
            focus: None,
            pruned_at: None,
            blink_requested_at: None,
            messages: None,
            streaming: None,
            expanded_prompts: HashSet::new(),
            copied: None,
        }
    }
}

/// One pass's messages, at the message revision they were read at.
struct SharedMessages {
    revision: u64,
    messages: Rc<[Arc<ConversationMessage>]>,
}

/// The rows a conversation's messages resolve to.
///
/// Rebuilt when the messages change rather than on every pass: it walks every
/// message, and the list needs a row's content only while the row is on
/// screen. Indices rather than messages, so it holds nothing the conversation
/// has dropped.
struct Plan {
    message_revision: u64,
    busy: bool,
    rows: Rc<[RowPlan]>,
    /// The row each message heads, which is how a paragraph's key finds its
    /// row.
    heads: Rc<HashMap<Uuid, usize>>,
}

/// What the list was last told, so a pass can tell it only what changed.
#[derive(Default)]
struct Measured {
    /// The conversation revision the stamps were taken at. Nothing a row's
    /// height depends on can change without moving it.
    revision: Option<u64>,
    /// The text size the rows were measured at. Every row's height follows
    /// it, and nothing about the conversation says when it changed.
    text_size: Option<TextSizePreset>,
    stamps: Vec<RowStamp>,
}

impl ThreadMemory {
    /// Bring the list up to date with this pass's rows.
    fn measure(&mut self, rows: &Rows, snapshot: &Snapshot) {
        let text_size = global::current().text_size;
        if self.measured.text_size != Some(text_size) {
            if self.measured.text_size.is_some() {
                remeasure_all(&self.list);
            }
            self.measured.text_size = Some(text_size);
        }
        if self.measured.revision == Some(snapshot.revision)
            && self.measured.stamps.len() == rows.len()
        {
            return;
        }
        let stamps = rows.stamps(snapshot);
        splice_changed(&self.list, &self.measured.stamps, &stamps);
        self.measured.stamps = stamps;
        self.measured.revision = Some(snapshot.revision);
    }

    /// This pass's rows, from the plan when the messages have not changed.
    fn plan(&mut self, snapshot: &Snapshot) -> (Rc<[RowPlan]>, Rc<HashMap<Uuid, usize>>) {
        if let Some(plan) = self.plan.as_ref().filter(|plan| {
            plan.message_revision == snapshot.message_revision && plan.busy == snapshot.busy
        }) {
            return (plan.rows.clone(), plan.heads.clone());
        }
        let messages: Vec<&ConversationMessage> =
            snapshot.messages.iter().map(|message| &**message).collect();
        let rows: Rc<[RowPlan]> = plan_rows(&messages, snapshot.busy).into();
        let heads: Rc<HashMap<Uuid, usize>> = Rc::new(
            rows.iter()
                .enumerate()
                .filter_map(|(row, plan)| {
                    let head = match plan {
                        RowPlan::Message(index) => *index,
                        RowPlan::Activity { messages, .. } => messages.start,
                        RowPlan::Tail | RowPlan::Failure => return None,
                    };
                    Some((messages[head].id, row))
                })
                .collect(),
        );
        self.plan = Some(Plan {
            message_revision: snapshot.message_revision,
            busy: snapshot.busy,
            rows: rows.clone(),
            heads: heads.clone(),
        });
        (rows, heads)
    }
}

impl ViewMemory {
    fn next_frame(&mut self) -> u32 {
        self.frame = self.frame.wrapping_add(1);
        self.frame
    }

    fn thread(&mut self, conversation: Uuid) -> &mut ThreadMemory {
        self.threads
            .entry(conversation)
            .or_insert_with(|| ThreadMemory::new(Uuid::nil()))
    }

    /// Drop one conversation's memory, including its keyed disclosures.
    fn forget(&mut self, conversation: Uuid) {
        self.threads.remove(&conversation);
        let prefix = format!("{conversation}:");
        self.disclosures.retain(|key, _| !key.starts_with(&prefix));
        self.detail_limits
            .retain(|key, _| !key.starts_with(&prefix));
    }
}

thread_local! {
    static MEMORY: RefCell<ViewMemory> = RefCell::new(ViewMemory::default());
}

/// Run `f` over one conversation's memory, creating it on first use.
fn with_thread<R>(conversation: Uuid, f: impl FnOnce(&mut ThreadMemory) -> R) -> R {
    MEMORY.with(|memory| f(memory.borrow_mut().thread(conversation)))
}

/// The list a conversation is drawn into.
#[cfg(test)]
fn list_state(conversation: Uuid) -> ListState {
    with_thread(conversation, |thread| thread.list.clone())
}

/// Forget everything this view remembers about a thread.
///
/// Called when the thread is deleted. The memory is keyed by conversation, so
/// without this every deleted thread kept its list state, focus handle, and
/// shared messages for the life of the process.
pub(crate) fn forget_thread(agent: Uuid) {
    let conversations: Vec<Uuid> = MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        let conversations: Vec<Uuid> = memory
            .threads
            .iter()
            .filter(|(_, thread)| thread.agent == agent)
            .map(|(id, _)| *id)
            .collect();
        for conversation in &conversations {
            memory.forget(*conversation);
        }
        conversations
    });
    for conversation in conversations {
        selection::forget(&conversation.to_string());
    }
}

/// Whether the view still keeps anything for a conversation.
#[cfg(test)]
pub(crate) fn remembers(conversation: Uuid) -> bool {
    MEMORY.with(|memory| memory.borrow().threads.contains_key(&conversation))
}

/// Let go of the messages every other thread's last pass shared.
///
/// The scroll position and disclosures stay; only the handles go, and the
/// next visit takes them again in one pass.
fn release_hidden_messages(shown: Uuid) {
    MEMORY.with(|memory| {
        for (id, thread) in memory.borrow_mut().threads.iter_mut() {
            if *id != shown {
                thread.messages = None;
                thread.streaming = None;
            }
        }
    });
}

/// Drop memory for every conversation the state no longer holds.
///
/// The backstop for [`forget_thread`]: a thread can leave the state by more
/// than one path, and the one being drawn is the only conversation this pass
/// can vouch for. A thread has one live conversation, so another conversation
/// remembered for the same thread is stale too.
fn sweep_memory(snapshot: &Snapshot, cx: &App) {
    let Some(handle) = crate::AppHandle::get(cx) else {
        return;
    };
    let state = handle.0.read(cx);
    let stale: Vec<Uuid> = MEMORY.with(|memory| {
        memory
            .borrow()
            .threads
            .iter()
            .filter(|(id, thread)| {
                **id != snapshot.id
                    && (thread.agent == snapshot.agent_id
                        || !state.conversations.contains_key(&thread.agent))
            })
            .map(|(id, _)| *id)
            .collect()
    });
    if stale.is_empty() {
        return;
    }
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        for conversation in &stale {
            memory.forget(*conversation);
        }
    });
    for conversation in stale {
        selection::forget(&conversation.to_string());
    }
}

fn is_prompt_expanded(conversation: Uuid, message: Uuid) -> bool {
    with_thread(conversation, |thread| {
        thread.expanded_prompts.contains(&message)
    })
}

fn toggle_prompt(conversation: Uuid, message: Uuid) {
    with_thread(conversation, |thread| {
        if !thread.expanded_prompts.remove(&message) {
            thread.expanded_prompts.insert(message);
        }
    });
}

fn is_copied(conversation: Uuid, message: Uuid) -> bool {
    with_thread(conversation, |thread| thread.copied == Some(message))
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

fn detail_limit(key: &str) -> usize {
    MEMORY.with(|memory| {
        memory
            .borrow()
            .detail_limits
            .get(key)
            .copied()
            .unwrap_or(ACTIVITY_PAGE_SIZE)
    })
}

fn reveal_earlier_actions(key: &str) {
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        let limit = memory
            .detail_limits
            .entry(key.to_owned())
            .or_insert(ACTIVITY_PAGE_SIZE);
        *limit = limit.saturating_add(ACTIVITY_PAGE_SIZE);
    });
}

/// Forget the groups a conversation no longer has.
///
/// Keyed on the revisions rather than run every pass: pruning is a map rebuild,
/// and a stream produces a revision on every delta. `live` names every group
/// the conversation has, and is asked for only when a prune runs.
fn prune_disclosures(
    conversation: Uuid,
    message_revision: u64,
    streaming_revision: u64,
    live: impl FnOnce() -> HashSet<String>,
) {
    let due = with_thread(conversation, |thread| {
        thread.pruned_at != Some((message_revision, streaming_revision))
    });
    if !due {
        return;
    }
    let live = live();
    let prefix = format!("{conversation}:");
    MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        let thread = memory.thread(conversation);
        thread.pruned_at = Some((message_revision, streaming_revision));
        // Only this conversation's keys are judged, so another thread's open
        // groups survive being filtered.
        memory
            .disclosures
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
        memory
            .detail_limits
            .retain(|key, _| !key.starts_with(&prefix) || live.contains(key));
    });
}

// ── The row model ──────────────────────────────────────────────────────────

/// One row of the transcript, resolved from the messages without building any
/// of it.
///
/// Every row has one of these; only the rows on screen go on to become
/// elements.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowPlan {
    /// Prose, a code block, an attachment, or a question — rendered as written.
    /// An index into the snapshot's messages.
    Message(usize),
    /// Tool traffic, collapsed into a group the reader can open: the messages
    /// it covers, and its position in its turn, which its key is built from.
    Activity {
        messages: Range<usize>,
        index: usize,
        /// Whether this is the tail of a turn that is still running.
        is_active: bool,
    },
    /// The reply still being written, or the indicator standing in for one.
    Tail,
    /// The failed turn's banner.
    Failure,
}

/// Every row one pass shows: the messages' rows, then the live tail and the
/// failure banner when there are any.
#[derive(Clone)]
struct Rows {
    plan: Rc<[RowPlan]>,
    tail: bool,
    failure: bool,
}

impl Rows {
    fn len(&self) -> usize {
        self.plan.len() + usize::from(self.tail) + usize::from(self.failure)
    }

    fn get(&self, index: usize) -> Option<RowPlan> {
        if let Some(row) = self.plan.get(index) {
            return Some(row.clone());
        }
        match (index - self.plan.len(), self.tail, self.failure) {
            (0, true, _) => Some(RowPlan::Tail),
            (0, false, true) | (1, true, true) => Some(RowPlan::Failure),
            _ => None,
        }
    }

    fn tail_index(&self) -> Option<usize> {
        self.tail.then_some(self.plan.len())
    }

    /// What each row's height depends on, as far as the conversation decides.
    fn stamps(&self, snapshot: &Snapshot) -> Vec<RowStamp> {
        (0..self.len())
            .filter_map(|index| self.get(index))
            .map(|row| match row {
                RowPlan::Message(index) => {
                    let message = &snapshot.messages[index];
                    RowStamp::Message {
                        id: message.id,
                        message: Arc::downgrade(message),
                    }
                }
                RowPlan::Activity {
                    messages,
                    is_active,
                    ..
                } => {
                    let first = &snapshot.messages[messages.start];
                    RowStamp::Activity {
                        id: first.id,
                        first: Arc::downgrade(first),
                        messages: messages.len(),
                        is_active,
                    }
                }
                RowPlan::Tail => RowStamp::Tail(snapshot.streaming_revision),
                RowPlan::Failure => RowStamp::Failure(snapshot.error.clone(), snapshot.recovery),
            })
            .collect()
    }
}

/// What a row's height depends on, as far as the conversation decides it.
///
/// Messages are held weakly. A stamp must not keep alive a message its
/// conversation has dropped, and a `Weak` still tells a replaced message from
/// its successor after the first is freed, which a bare address cannot.
#[derive(Debug, Clone)]
enum RowStamp {
    Message {
        id: Uuid,
        message: Weak<ConversationMessage>,
    },
    Activity {
        id: Uuid,
        first: Weak<ConversationMessage>,
        messages: usize,
        is_active: bool,
    },
    Tail(u64),
    Failure(Option<String>, RecoveryFacts),
}

impl RowStamp {
    /// Whether two stamps are the same row, changed or not.
    fn same_row(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message { id: a, .. }, Self::Message { id: b, .. })
            | (Self::Activity { id: a, .. }, Self::Activity { id: b, .. }) => a == b,
            (Self::Tail(_), Self::Tail(_)) | (Self::Failure(..), Self::Failure(..)) => true,
            _ => false,
        }
    }
}

impl PartialEq for RowStamp {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message { message: a, .. }, Self::Message { message: b, .. }) => {
                Weak::ptr_eq(a, b)
            }
            (
                Self::Activity {
                    first: a,
                    messages: a_messages,
                    is_active: a_active,
                    ..
                },
                Self::Activity {
                    first: b,
                    messages: b_messages,
                    is_active: b_active,
                    ..
                },
            ) => Weak::ptr_eq(a, b) && a_messages == b_messages && a_active == b_active,
            (Self::Tail(a), Self::Tail(b)) => a == b,
            (Self::Failure(a, a_facts), Self::Failure(b, b_facts)) => a == b && a_facts == b_facts,
            _ => false,
        }
    }
}

/// Tell the list which rows changed since it last measured them.
///
/// One splice over the stretch between the longest unchanged prefix and
/// suffix: a streamed delta touches only the tail, a new message only the
/// end, a trimmed one only the start. Everything else keeps its measured
/// height.
///
/// A reader scrolled into a row that changed keeps their place in it. GPUI
/// moves a splice's scroll anchor to the top of the replaced stretch, which
/// for a reader halfway up a reply still streaming in would be a jump on
/// every delta.
fn splice_changed(list: &ListState, old: &[RowStamp], new: &[RowStamp]) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let replaced = prefix..old.len() - suffix;
    let count = new.len() - prefix - suffix;
    if replaced.is_empty() && count == 0 {
        return;
    }
    #[cfg(test)]
    tests::SPLICES.with(|splices| splices.borrow_mut().push((replaced.clone(), count)));
    let top = list.logical_scroll_top();
    let anchored = top.item_ix < list.item_count();
    list.splice(replaced.clone(), count);
    if anchored && replaced.contains(&top.item_ix) {
        let row = &old[top.item_ix];
        if let Some(at) = new[prefix..prefix + count]
            .iter()
            .position(|stamp| stamp.same_row(row))
        {
            list.scroll_to(ListOffset {
                item_ix: prefix + at,
                offset_in_item: top.offset_in_item,
            });
        }
    }
}

/// Forget every row's height, keeping the reader's place.
fn remeasure_all(list: &ListState) {
    let count = list.item_count();
    let top = list.logical_scroll_top();
    #[cfg(test)]
    tests::SPLICES.with(|splices| splices.borrow_mut().push((0..count, count)));
    list.splice(0..count, count);
    if top.item_ix < count {
        list.scroll_to(top);
    }
}

/// One thing the transcript shows, resolved from a contiguous run of messages.
#[cfg(test)]
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
fn plan_rows(messages: &[&ConversationMessage], turn_is_running: bool) -> Vec<RowPlan> {
    let mut rows = Vec::new();
    let mut turn_start = 0;
    for (index, message) in messages.iter().enumerate() {
        if message.role == MessageRole::User {
            plan_turn(&mut rows, messages, turn_start..index, false);
            rows.push(RowPlan::Message(index));
            turn_start = index + 1;
        }
    }
    plan_turn(
        &mut rows,
        messages,
        turn_start..messages.len(),
        turn_is_running,
    );
    rows
}

/// Hand one assistant turn to the grouping policy and take the rows it names.
fn plan_turn(
    rows: &mut Vec<RowPlan>,
    messages: &[&ConversationMessage],
    turn: Range<usize>,
    is_running: bool,
) {
    if turn.is_empty() {
        return;
    }
    // The policy partitions the borrowed turn directly. Copying the turn into a
    // slice for it would deep-copy every tool output and image in the turn on
    // every pass — which is what this did, under the conversation's lock, while
    // the runner waited on that lock for each streamed delta.
    let runs = TranscriptActivityGroupingPolicy::runs(messages[turn.clone()].iter().copied());
    let last = runs.len().saturating_sub(1);
    for (index, (range, is_activity)) in runs.into_iter().enumerate() {
        let range = turn.start + range.start..turn.start + range.end;
        if !is_activity {
            rows.extend(range.map(RowPlan::Message));
            continue;
        }
        // Only the last segment of a running turn is live. Anything else has
        // already been overtaken by what follows it, and holding it open would
        // leave a spinner running on settled work.
        rows.push(RowPlan::Activity {
            messages: range,
            index,
            is_active: is_running && index == last,
        });
    }
}

/// The group an activity row names, built only while the row is drawn or its
/// text is read.
fn group_for<'a>(
    conversation: Uuid,
    messages: &[&'a ConversationMessage],
    index: usize,
    is_active: bool,
) -> ActivityGroup<'a> {
    ActivityGroup {
        key: activity_key(conversation, messages, index),
        calls: tool_calls(messages),
        summary: summarize(messages),
        is_active,
    }
}

/// An activity row's messages, borrowed out of the snapshot.
fn group_messages<'a>(
    snapshot: &'a Snapshot,
    messages: &Range<usize>,
) -> Vec<&'a ConversationMessage> {
    snapshot.messages[messages.clone()]
        .iter()
        .map(|message| &**message)
        .collect()
}

/// The rows [`plan_rows`] names, with each group built.
#[cfg(test)]
fn build_rows<'a>(
    messages: &[&'a ConversationMessage],
    turn_is_running: bool,
    conversation: Uuid,
) -> Vec<Row<'a>> {
    plan_rows(messages, turn_is_running)
        .into_iter()
        .filter_map(|row| match row {
            RowPlan::Message(index) => Some(Row::Message(messages[index])),
            RowPlan::Activity {
                messages: range,
                index,
                is_active,
            } => Some(Row::Activity(group_for(
                conversation,
                &messages[range],
                index,
                is_active,
            ))),
            RowPlan::Tail | RowPlan::Failure => None,
        })
        .collect()
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

// ── The snapshot ───────────────────────────────────────────────────────────

/// Everything one pass reads from a conversation.
///
/// Taken under the conversation's lock, which is then released before any
/// element is built: the runner takes the same lock for every streamed delta,
/// and a lock held across Markdown parsing, shaping, and grouping would stall
/// the provider's side as well as this one.
pub(crate) struct Snapshot {
    id: Uuid,
    agent_id: Uuid,
    /// The conversation's own messages, shared rather than copied, and the same
    /// list as the previous pass's until a message is added or replaced.
    messages: Rc<[Arc<ConversationMessage>]>,
    /// Moves whenever anything a row draws changes.
    revision: u64,
    message_revision: u64,
    streaming_revision: u64,
    streaming: Rc<str>,
    phase: Phase,
    busy: bool,
    waiting_for_user: bool,
    error: Option<String>,
    approvals: Vec<PendingApproval>,
    questions: Vec<PendingQuestion>,
    /// Calls in the trailing turn that the engine saw finish without a result
    /// worth recording. Only a live group asks, so nothing earlier is copied.
    completed_tools: HashSet<String>,
    recovery: RecoveryFacts,
}

impl Snapshot {
    fn is_streaming(&self) -> bool {
        !self.streaming.is_empty()
    }

    fn has_completed_tool(&self, id: &str) -> bool {
        self.completed_tools.contains(id)
    }
}

/// What a failed turn's banner needs to know to offer a way out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RecoveryFacts {
    /// A prompt is still waiting for the provider to accept it.
    unsent: bool,
    /// The thread has a provider session to resume or restart.
    session: bool,
    /// The reader has sent a prompt that a retry could send again.
    prompt: bool,
}

/// Read a conversation into a snapshot.
pub(crate) fn snapshot(conversation: &crate::Conversation) -> Snapshot {
    let message_revision = conversation.message_revision();
    let streaming_revision = conversation.streaming_revision();
    let (messages, streaming) = with_thread(conversation.id, |thread| {
        thread.agent = conversation.agent_id;
        let messages = match thread
            .messages
            .as_ref()
            .filter(|shared| shared.revision == message_revision)
        {
            Some(shared) => shared.messages.clone(),
            None => {
                let messages: Rc<[Arc<ConversationMessage>]> =
                    conversation.shared_messages().cloned().collect();
                thread.messages = Some(SharedMessages {
                    revision: message_revision,
                    messages: messages.clone(),
                });
                messages
            }
        };
        let streaming = match &thread.streaming {
            Some((revision, text)) if *revision == streaming_revision => text.clone(),
            _ => {
                let text: Rc<str> = conversation.streaming_text().into();
                thread.streaming = Some((streaming_revision, text.clone()));
                text
            }
        };
        (messages, streaming)
    });

    let busy = conversation.is_busy();
    let completed_tools = if busy {
        messages
            .iter()
            .rev()
            .take_while(|message| message.role != MessageRole::User)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                MessageContent::ToolUse { id, .. } if conversation.has_completed_tool(id) => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect()
    } else {
        HashSet::new()
    };
    let error = conversation.error().map(str::to_owned);
    let recovery = if error.is_some() {
        RecoveryFacts {
            unsent: conversation.unsent().is_some(),
            session: conversation.session_id().is_some_and(|id| !id.is_empty()),
            prompt: messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
                .is_some_and(|message| !message.text_content().trim().is_empty()),
        }
    } else {
        RecoveryFacts::default()
    };

    Snapshot {
        id: conversation.id,
        agent_id: conversation.agent_id,
        messages,
        revision: conversation.revision(),
        message_revision,
        streaming_revision,
        streaming,
        phase: conversation.phase(),
        busy,
        waiting_for_user: conversation.is_waiting_for_user(),
        error,
        approvals: conversation.pending_approvals().to_vec(),
        questions: conversation.pending_questions().to_vec(),
        completed_tools,
        recovery,
    }
}

// ── Entry point ────────────────────────────────────────────────────────────

/// Render one conversation's transcript.
pub fn view(conversation: &crate::Conversation, window: &mut Window, cx: &mut App) -> AnyElement {
    let snapshot = snapshot(conversation);
    render(snapshot, window, cx)
}

/// Render a conversation that a running turn may be writing into.
///
/// The lock is held only while the snapshot is taken.
pub fn view_guarded(
    conversation: &Mutex<crate::Conversation>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let snapshot = {
        // A poisoned mutex means a turn panicked while writing. The transcript
        // is still readable, and the error is reported by the session layer
        // rather than by refusing to draw.
        let conversation = conversation.lock().unwrap_or_else(PoisonError::into_inner);
        snapshot(&conversation)
    };
    render(snapshot, window, cx)
}

fn render(snapshot: Snapshot, window: &mut Window, cx: &mut App) -> AnyElement {
    let theme = global::current();
    sweep_memory(&snapshot, cx);
    release_hidden_messages(snapshot.id);
    crate::markdown::begin_pass();
    // Shared with the list, which asks for rows after this function returns.
    let snapshot = Rc::new(snapshot);

    let (plan, heads) = with_thread(snapshot.id, |thread| thread.plan(&snapshot));
    // Only the last segment of the last turn can be live, so only the last
    // row needs looking at.
    let has_active_group = matches!(
        plan.last(),
        Some(RowPlan::Activity {
            is_active: true,
            ..
        })
    );
    let rows = Rows {
        plan,
        tail: has_tail(&snapshot, has_active_group),
        failure: snapshot.error.is_some(),
    };
    prune_disclosures(
        snapshot.id,
        snapshot.message_revision,
        snapshot.streaming_revision,
        || live_groups(&rows.plan, &snapshot),
    );

    let frame = MEMORY.with(|memory| memory.borrow_mut().next_frame());
    request_caret_frame(&snapshot, frame, window);
    let (state, scrolled) = with_thread(snapshot.id, |thread| {
        // Before this pass's splices, so the heights near the bottom are the
        // ones the last layout measured.
        follow_bottom(&thread.list, &thread.scrolled);
        thread.measure(&rows, &snapshot);
        (thread.list.clone(), thread.scrolled.clone())
    });
    let focus = viewport_focus(snapshot.id, || {
        let mut handle = cx.focus_handle();
        // Reachable from the keyboard at all. Without this the viewport is
        // focusable in principle and unreachable in practice.
        handle.tab_stop = true;
        handle
    });
    let document: Rc<dyn selection::Document> = Rc::new(TranscriptDocument {
        snapshot: snapshot.clone(),
        rows: rows.clone(),
        heads,
    });

    let content = if rows.len() == 0 {
        // Give the empty state the same viewport as a populated conversation.
        // Padding alone leaves it stranded at the top of a tall window.
        div()
            .flex_1()
            .w_full()
            .flex()
            .flex_col()
            .justify_center()
            .child(reading_column(empty_transcript_notice()))
            .into_any_element()
    } else {
        let (for_rows, for_snapshot) = (rows.clone(), snapshot.clone());
        list(state.clone(), move |index, _window, cx| {
            match for_rows.get(index) {
                Some(row) => row_element(&row, &for_snapshot, has_active_group, frame, cx),
                // Asked for past the end only if the list was told of a row that
                // is gone; an empty row is the honest answer.
                None => div().into_any_element(),
            }
        })
        .flex_1()
        .w_full()
        .pt(px(LIST_PADDING))
        .pb(px(LIST_PADDING))
        .into_any_element()
    };

    let viewport = div()
        // `.id` is what upgrades a `Div` to `Stateful<Div>`, which is where
        // focus tracking lives in GPUI 0.2.2.
        .id(SCROLL_ID)
        .debug_selector(|| SCROLL_ID.to_string())
        .flex_1()
        .min_h_0()
        .w_full()
        .flex()
        .flex_col()
        // `track_focus` also makes the element focusable, which is what
        // lets the key handler below be reached at all — and a press on
        // the text focuses it, which is what brings Cmd-C here.
        .track_focus(&focus)
        .on_key_down(move |event, window, cx| {
            if selection::handle_key(event, cx) {
                cx.stop_propagation();
                window.refresh();
                return;
            }
            if !page_with_keyboard(&state, event) {
                // Not a key this viewport claims. Stopping propagation
                // anyway would swallow Tab and every shortcut while it
                // had focus.
                return;
            }
            scrolled.set(true);
            // Claimed rather than passed on: an arrow key that also
            // reached the composer would move the caret while the
            // transcript scrolled.
            cx.stop_propagation();
            // Moving the list is not enough on its own — nothing has
            // asked for a frame yet.
            window.refresh();
        })
        .child(content);

    div()
        .w_full()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .bg(Colors::content_bg(&theme))
        .child(selection::scope(
            snapshot.id.to_string(),
            Some(document),
            viewport,
        ))
        .child(pending_prompts(&snapshot))
        .into_any_element()
}

/// Build one row, on its reading column.
///
/// The list calls this only for rows in view, and for rows near it whose
/// height it does not know yet. Everything per row — Markdown, shaping,
/// grouping a burst of tool calls — happens here and nowhere else.
fn row_element(
    row: &RowPlan,
    snapshot: &Snapshot,
    has_active_group: bool,
    frame: u32,
    cx: &App,
) -> AnyElement {
    #[cfg(test)]
    tests::ROWS_BUILT.with(|built| built.set(built.get() + 1));
    let content = match row {
        RowPlan::Message(index) => message_row(&snapshot.messages[*index], snapshot, cx),
        RowPlan::Activity {
            messages,
            index,
            is_active,
        } => {
            let messages = group_messages(snapshot, messages);
            activity_group(
                &group_for(snapshot.id, &messages, *index, *is_active),
                snapshot,
                frame,
            )
        }
        RowPlan::Tail => streaming_tail(snapshot, frame, has_active_group)
            .unwrap_or_else(|| div().into_any_element()),
        RowPlan::Failure => match snapshot.error.as_deref() {
            Some(error) => error_banner(error, snapshot),
            None => div().into_any_element(),
        },
    };
    reading_column(content)
}

/// Centre a row on the transcript's reading measure.
fn reading_column(content: AnyElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .justify_center()
        .child(
            div()
                .debug_selector(|| COLUMN_SELECTOR.to_string())
                .w_full()
                .min_w_0()
                .max_w(px(Layout::READABLE_CONTENT_WIDTH))
                .px(px_len(Spacing::XXL))
                .flex()
                .flex_col()
                .child(content),
        )
        .into_any_element()
}

/// The key of every activity group the plan names, for pruning the ones that
/// went away.
fn live_groups(plan: &[RowPlan], snapshot: &Snapshot) -> HashSet<String> {
    plan.iter()
        .filter_map(|row| match row {
            RowPlan::Activity {
                messages, index, ..
            } => Some(activity_key(
                snapshot.id,
                &[&*snapshot.messages[messages.start]],
                *index,
            )),
            _ => None,
        })
        .collect()
}

/// The focus handle for a conversation's viewport, made by `make` only the
/// first time a thread asks.
///
/// One per thread and kept until the thread is forgotten: a handle made fresh
/// each pass would orphan the focused one, and focus would be lost on every
/// repaint. Making one eagerly and discarding it was the same allocation every
/// pass, for nothing.
fn viewport_focus(conversation: Uuid, make: impl FnOnce() -> FocusHandle) -> FocusHandle {
    with_thread(conversation, |thread| {
        thread.focus.get_or_insert_with(make).clone()
    })
}

/// Move the viewport a page or a line at a time.
///
/// GPUI 0.2.2 gives a scrollable element no key handling whatsoever, so the
/// arrows and page keys go nowhere unless something claims them. The original
/// inherited this from `NSScrollView` without writing a line for it, so leaving
/// it out would drop a capability rather than a convenience.
fn page_with_keyboard(list: &ListState, event: &KeyDownEvent) -> bool {
    match event.keystroke.key.as_str() {
        "home" => list.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        }),
        // Past the last row is the bottom: the list pins itself there, and
        // follows whatever arrives next.
        "end" => list.scroll_to(ListOffset {
            item_ix: list.item_count(),
            offset_in_item: px(0.0),
        }),
        key => {
            let viewport = f64::from(list.viewport_bounds().size.height);
            // A viewport that has not been laid out yet has no height, so a page
            // step derived from it would be nonsense.
            if viewport <= 1.0 {
                return false;
            }
            let line = f64::from(Typography::body_point_size(&global::current())) * 3.0;
            let page = viewport * 0.9;
            let step = match key {
                "pagedown" => page,
                "pageup" => -page,
                "down" => line,
                "up" => -line,
                _ => return false,
            };
            let before = list.logical_scroll_top();
            if before.item_ix >= list.item_count() {
                // Pinned. Nothing is below, and the list's own idea of where
                // it is — the very end — sits a viewport below what is on
                // screen, so a step from there would barely move. Start from
                // what the reader can see.
                if step > 0.0 {
                    return false;
                }
                let content = -f64::from(list.scroll_px_offset_for_scrollbar().y);
                let shown = (content + f64::from(LIST_PADDING) * 2.0 - viewport).max(0.0);
                list.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.0),
                });
                list.scroll_by(px(shown as f32));
            }
            list.scroll_by(px(step as f32));
            let after = list.logical_scroll_top();
            // Already at the end it was moving towards: nothing moved, so
            // nothing is claimed.
            if before.item_ix == after.item_ix
                && (f64::from(before.offset_in_item) - f64::from(after.offset_in_item)).abs() < 1.0
            {
                return false;
            }
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
/// The request is keyed on the blink *phase*, not on every pass. The rows in
/// view are rebuilt from scratch on each render, and the original went to some
/// length to avoid exactly that per delta, so asking for a frame per pass would
/// undo the saving. Keying on the phase bounds it to one extra frame every
/// `CARET_BLINK_PASSES` passes, however fast the stream runs.
fn request_caret_frame(snapshot: &Snapshot, frame: u32, window: &mut Window) {
    if !snapshot.is_streaming() {
        return;
    }
    let phase = blink_phase(frame);
    let wanted = with_thread(snapshot.id, |thread| {
        if thread.blink_requested_at == Some(phase) {
            return false;
        }
        thread.blink_requested_at = Some(phase);
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

/// Pin the view to the bottom when the reader has scrolled back to it.
///
/// The list pins itself only when scrolled exactly to its end, and stays
/// pinned through every delta after that. Whether a reader who stopped just
/// short of the end is following too is `ConversationScrollPolicy`'s call, as
/// it always was; this only supplies the geometry the last layout measured.
///
/// Asked only after the reader scrolled. Rows the list has not measured count
/// as nothing, and the rows below the viewport are measured at least
/// `OVERDRAW` down, so a distant bottom cannot read as a near one.
fn follow_bottom(list: &ListState, scrolled: &Cell<bool>) {
    if !scrolled.replace(false) {
        return;
    }
    let count = list.item_count();
    if list.logical_scroll_top().item_ix >= count {
        return;
    }
    // The scrollbar's range leaves out the list's padding; the list's own
    // scrolling does not.
    let range = list.max_offset_for_scrollbar().height + px(LIST_PADDING * 2.0);
    let pinned = ConversationScrollPolicy::is_pinned(ScrollMetrics {
        offset: -f32::from(list.scroll_px_offset_for_scrollbar().y),
        max_offset: f32::from(range),
    });
    if pinned {
        list.scroll_to(ListOffset {
            item_ix: count,
            offset_in_item: px(0.0),
        });
    }
}

// ── Messages ───────────────────────────────────────────────────────────────

/// Where a content block sits. Every selection key and menu owner below is
/// built from this, so the two cannot drift apart.
#[derive(Debug, Clone, Copy)]
struct Place {
    conversation: Uuid,
    message: Uuid,
    index: usize,
}

impl Place {
    /// A selection key for one part of this block.
    ///
    /// Every key starts with the message id, which is how a row asks whether
    /// the selection touches it.
    fn key(&self, part: &str) -> SharedString {
        SharedString::from(format!("{}:{}:{part}", self.message, self.index))
    }
}

/// The context-menu owner for a message, or for one image in it.
fn menu_owner(conversation: Uuid, message: Uuid, image: Option<ImageSlot>) -> String {
    match image {
        None => format!("{MENU_OWNER_PREFIX}{conversation}:{message}"),
        Some(ImageSlot::Content(index)) => {
            format!("{MENU_OWNER_PREFIX}{conversation}:{message}:image:{index}")
        }
        Some(ImageSlot::Mentioned(index)) => {
            format!("{MENU_OWNER_PREFIX}{conversation}:{message}:mentioned:{index}")
        }
    }
}

/// Which image of a message a menu was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageSlot {
    /// An image block, by its index in the message's content.
    Content(usize),
    /// A local image the prompt only mentions, by its position among them.
    Mentioned(usize),
}

/// What a transcript menu was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuTarget {
    conversation: Uuid,
    message: Uuid,
    image: Option<ImageSlot>,
}

impl MenuTarget {
    fn parse(owner: &str) -> Option<Self> {
        let mut parts = owner.strip_prefix(MENU_OWNER_PREFIX)?.split(':');
        let conversation = parts.next()?.parse().ok()?;
        let message = parts.next()?.parse().ok()?;
        let image = match (parts.next(), parts.next()) {
            (None, _) => None,
            (Some("image"), Some(index)) => Some(ImageSlot::Content(index.parse().ok()?)),
            (Some("mentioned"), Some(index)) => Some(ImageSlot::Mentioned(index.parse().ok()?)),
            _ => return None,
        };
        Some(Self {
            conversation,
            message,
            image,
        })
    }
}

/// Wrap `content` in a context-menu region, when the shell is there to own one.
fn with_menu(
    owner: String,
    menu: Option<MenuModel>,
    content: impl IntoElement,
    cx: &App,
) -> AnyElement {
    match crate::AppHandle::get(cx) {
        Some(handle) => {
            let registry = handle.0.read(cx).context_registry.clone();
            context_region(&registry, owner, menu, content)
        }
        None => content.into_any_element(),
    }
}

/// A message's context menu: the selection first when it touches the message,
/// then the whole message.
fn message_menu(message: &ConversationMessage) -> Option<MenuModel> {
    let selected = selection::touches(&format!("{}:", message.id));
    let mut sections = Vec::new();
    if selected {
        sections.push(MenuSection::new(
            "selection",
            vec![MenuItem::new(COPY_SELECTION, "Copy")],
        ));
    }
    if has_copyable_text(message) {
        sections.push(MenuSection::new(
            "message",
            vec![MenuItem::new(
                COPY_MESSAGE,
                if selected { "Copy Message" } else { "Copy" },
            )],
        ));
    }
    (!sections.is_empty()).then(|| MenuModel::new(sections))
}

fn image_menu() -> MenuModel {
    MenuModel::new(vec![MenuSection::new(
        "image",
        vec![MenuItem::new(COPY_IMAGE, "Copy Image")],
    )])
}

/// One transcript row. A bubble for the reader's own prompts, prose for the rest.
fn message_row(message: &ConversationMessage, snapshot: &Snapshot, cx: &App) -> AnyElement {
    let theme = global::current();
    let is_user = message.role == MessageRole::User;
    let conversation = snapshot.id;

    let paths = crate::AppHandle::get(cx).map(|handle| handle.0.read(cx).paths.clone());
    let mentioned = if is_user {
        mentioned_attachments(message)
    } else {
        Vec::new()
    };
    let image_mentions: Vec<_> = mentioned.iter().filter(|file| file.is_image).collect();
    let mut image_index = 0;
    let mut blocks = Vec::new();
    for (index, block) in message.content.iter().enumerate() {
        let clean_text = match block {
            MessageContent::Text { text } if is_user => Some(MessageContent::Text {
                text: user_prompt(text).text.to_string(),
            }),
            _ => None,
        };
        let image_mention = if block.is_image() {
            let mention = image_mentions.get(image_index).copied();
            image_index += 1;
            mention
        } else {
            None
        };
        let place = Place {
            conversation,
            message: message.id,
            index,
        };
        if let Some(element) = content_block(
            clean_text.as_ref().unwrap_or(block),
            place,
            paths.as_ref(),
            image_mention,
            !is_user,
            cx,
        ) {
            blocks.push(element);
        }
    }
    // Native histories may carry only the attachment envelope. Its explicit
    // local image references recover those previews without duplicating the
    // image blocks supplied by providers that include both representations.
    for (index, attachment) in image_mentions.iter().enumerate().skip(image_index) {
        blocks.push(with_menu(
            menu_owner(conversation, message.id, Some(ImageSlot::Mentioned(index))),
            Some(image_menu()),
            image_attachment(
                SharedString::from(format!("attachment:{}:mentioned:{index}", message.id)),
                ImageInput::Mentioned(attachment.path.clone()),
                attachment.name.clone(),
            ),
            cx,
        ));
    }
    for attachment in mentioned.iter().filter(|file| !file.is_image) {
        blocks.push(
            div()
                .min_w_0()
                .whitespace_normal()
                .fx_caption()
                .child(SharedString::from(format!(
                    "{} — {}",
                    attachment.name,
                    attachment.path.display()
                )))
                .into_any_element(),
        );
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
        .pb(px_len(Spacing::XXL))
        .id(SharedString::from(format!("message:{}", message.id)));
    let row = if is_user {
        let message_id = message.id;
        column
            .items_end()
            .group(USER_ROW_GROUP)
            // The confirmation belongs to the hover that produced it. Leaving
            // it up would claim the next copy had already happened.
            .on_hover(move |hovered, window, _cx| {
                if !*hovered
                    && with_thread(conversation, |thread| {
                        thread
                            .copied
                            .take_if(|copied| *copied == message_id)
                            .is_some()
                    })
                {
                    window.refresh();
                }
            })
            // The button hangs off the bubble's leading edge rather than
            // sharing a row with it. A row would make the bubble a shrinkable
            // flex item, and a wrapped prompt would size itself to its longest
            // word.
            .child(bubble.relative().children(
                has_copyable_text(message).then(|| copy_message_button(snapshot, message)),
            ))
    } else {
        column.items_start().child(bubble)
    };
    with_menu(
        menu_owner(conversation, message.id, None),
        message_menu(message),
        row,
        cx,
    )
}

/// The attachments a prompt's envelope mentions.
fn mentioned_attachments(message: &ConversationMessage) -> Vec<MentionedAttachment> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            MessageContent::Text { text } => Some(user_prompt(text).attachments),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The hover button that copies one of the reader's prompts.
///
/// Hidden until the row is hovered, and only then reachable: the button sits
/// inside the hovered row, so a pointer over it has already revealed it.
fn copy_message_button(snapshot: &Snapshot, message: &ConversationMessage) -> AnyElement {
    let conversation = snapshot.id;
    let message_id = message.id;
    let copied = is_copied(conversation, message_id);
    div()
        .debug_selector(|| MESSAGE_COPY_SELECTOR.to_string())
        .absolute()
        .top_0()
        .right(gpui::relative(1.0))
        .mr(px_len(Spacing::SM))
        .opacity(0.0)
        .group_hover(USER_ROW_GROUP, |style| style.fx_opacity(1.0))
        .child(compact_action_button(
            if copied { "Copied" } else { "Copy" },
            copied.then_some(Icon::CHECKMARK),
            move |_, window, cx| {
                if copy_message(conversation, message_id, cx) {
                    with_thread(conversation, |thread| thread.copied = Some(message_id));
                    window.refresh();
                }
            },
        ))
        .into_any_element()
}

/// One content block, or `None` when it has nothing to show.
fn content_block(
    block: &MessageContent,
    place: Place,
    paths: Option<&fx_store::AppPaths>,
    mention: Option<&MentionedAttachment>,
    markdown: bool,
    cx: &App,
) -> Option<AnyElement> {
    let image = |input: ImageInput| {
        with_menu(
            menu_owner(
                place.conversation,
                place.message,
                Some(ImageSlot::Content(place.index)),
            ),
            Some(image_menu()),
            image_attachment(
                SharedString::from(format!("attachment:{}:{}", place.message, place.index)),
                input,
                mention
                    .map(|file| file.name.clone())
                    .unwrap_or_else(|| "Image attachment".into()),
            ),
            cx,
        )
    };
    match block {
        MessageContent::Text { text } => {
            if text.trim().is_empty() {
                return None;
            }
            if markdown {
                return Some(crate::markdown::render(
                    text,
                    format!("{}:{}:md", place.message, place.index),
                ));
            }
            Some(prompt_text(text, place))
        }
        MessageContent::Code { language, code } => {
            let trimmed = language.trim();
            let label = if trimmed.is_empty() {
                "Code".to_string()
            } else {
                trimmed.to_ascii_uppercase()
            };
            Some(mono_panel(
                &label,
                code,
                BlockTone::Standard,
                place.key("code"),
                true,
            ))
        }
        MessageContent::Image { data, .. } => Some(image(ImageInput::inline(data))),
        MessageContent::ImageAsset(reference) => Some(image(ImageInput::asset(paths, reference))),
        MessageContent::ToolUse { name, input, .. } => {
            if name.eq_ignore_ascii_case(QUESTION_TOOL_NAME) {
                Some(question_card(input, place))
            } else {
                Some(inline_tool_call(name, input, place))
            }
        }
        MessageContent::ToolResult {
            content, is_error, ..
        } => Some(inline_tool_result(content, *is_error, place)),
    }
}

/// Whether a prompt is long enough to collapse behind "Show more".
fn can_collapse(text: &str) -> bool {
    text.chars().nth(COLLAPSED_CHARACTER_LIMIT).is_some()
        || text.split('\n').nth(COLLAPSED_LINE_LIMIT).is_some()
}

/// The part of a collapsed prompt worth shaping.
fn collapsed_preview(text: &str) -> &str {
    match text.char_indices().nth(COLLAPSED_PREVIEW_CHARACTERS) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// What a prompt shows: all of it, or the preview while it is collapsed.
///
/// Returns whether it can collapse and whether it is expanded too.
fn prompt_shown(text: &str, place: Place) -> (SharedString, bool, bool) {
    let collapsible = can_collapse(text);
    let expanded = collapsible && is_prompt_expanded(place.conversation, place.message);
    let shown = SharedString::from(if collapsible && !expanded {
        collapsed_preview(text).to_string()
    } else {
        text.to_string()
    });
    (shown, collapsible, expanded)
}

/// The text of one of the reader's prompts, collapsed when it is long.
fn prompt_text(text: &str, place: Place) -> AnyElement {
    let (shown, collapsible, expanded) = prompt_shown(text, place);
    let body = div()
        .debug_selector(|| PROMPT_BODY_SELECTOR.to_string())
        .min_w_0()
        .whitespace_normal()
        .fx_body()
        .child(SelectableText::new(
            place.key("text"),
            shown.clone(),
            StyledText::new(shown),
        ));
    if !collapsible {
        return body.into_any_element();
    }
    let body = if expanded {
        body
    } else {
        // Clipped rather than cut at a line count: the bubble wraps, so only
        // the laid-out height says how much of the prompt fits.
        body.max_h(px(Layout::COLLAPSED_USER_MESSAGE_HEIGHT))
            .overflow_hidden()
    };
    let (conversation, message) = (place.conversation, place.message);
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px_len(Spacing::XS))
        .child(body)
        .child(
            div()
                .debug_selector(|| PROMPT_TOGGLE_SELECTOR.to_string())
                .child(text_disclosure(
                    if expanded {
                        "Show less"
                    } else {
                        "Show full message"
                    },
                    expanded,
                    move |_, window, _cx| {
                        toggle_prompt(conversation, message);
                        window.refresh();
                    },
                )),
        )
        .into_any_element()
}

/// A tool call rendered inside a narrative row, because it shares a message with
/// prose and collapsing it would bury the words the model just said.
fn inline_tool_call(name: &str, input: &str, place: Place) -> AnyElement {
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
        block = block.child(mono_panel(
            "Input",
            input,
            BlockTone::Standard,
            place.key("input"),
            false,
        ));
    }
    block.into_any_element()
}

/// A tool result rendered inside a narrative row.
///
/// An error result is always recorded — that is the whole point of recording it
/// — so it has to be noticeable even when it is the only thing in its row. The
/// tint is the wash token rather than the hue at low alpha, because that is what
/// the rest of the app uses for a failure.
fn inline_tool_result(content: &str, is_error: bool, place: Place) -> AnyElement {
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
        .child(mono_panel(
            "Output",
            content,
            if is_error {
                BlockTone::Failure
            } else {
                BlockTone::Standard
            },
            place.key("output"),
            false,
        ))
        .into_any_element()
}

// ── Copying ────────────────────────────────────────────────────────────────

/// Whether a message has any text a copy would carry.
///
/// Asked on every pass for every row, so it inspects rather than builds.
fn has_copyable_text(message: &ConversationMessage) -> bool {
    message.content.iter().any(|block| match block {
        MessageContent::Text { text } => !text.trim().is_empty(),
        MessageContent::Code { code, .. } => !code.is_empty(),
        MessageContent::ToolUse { .. } | MessageContent::ToolResult { .. } => true,
        MessageContent::Image { .. } | MessageContent::ImageAsset(_) => false,
    })
}

/// A whole message as text, the way the original copied it.
///
/// The reader's prompts lose their attachment envelope, as they do on screen;
/// the model's prose keeps its Markdown, which is what makes a pasted answer
/// keep its structure.
fn copyable_text(message: &ConversationMessage) -> String {
    let is_user = message.role == MessageRole::User;
    let parts: Vec<String> = message
        .content
        .iter()
        .filter_map(|block| match block {
            MessageContent::Text { text } if is_user => Some(user_prompt(text).text.to_string()),
            MessageContent::Text { text } => Some(text.clone()),
            MessageContent::Code { code, .. } => Some(code.clone()),
            MessageContent::ToolUse { name, input, .. } => Some(format!("{name}\n{input}")),
            MessageContent::ToolResult { content, .. } => Some(content.clone()),
            MessageContent::Image { .. } | MessageContent::ImageAsset(_) => None,
        })
        .filter(|part| !part.trim().is_empty())
        .collect();
    parts.join("\n\n")
}

/// The message a menu or button refers to, from the last pass's snapshot.
///
/// Read from what was drawn rather than from the conversation: the click is
/// about the row the reader saw, and this needs no lock.
fn drawn_message(conversation: Uuid, message: Uuid) -> Option<Arc<ConversationMessage>> {
    MEMORY.with(|memory| {
        let memory = memory.borrow();
        let shared = memory.threads.get(&conversation)?.messages.as_ref()?;
        shared
            .messages
            .iter()
            .find(|candidate| candidate.id == message)
            .cloned()
    })
}

fn copy_message(conversation: Uuid, message: Uuid, cx: &mut App) -> bool {
    let Some(message) = drawn_message(conversation, message) else {
        return false;
    };
    let text = copyable_text(&message);
    if text.is_empty() {
        return false;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    true
}

/// The source of one image in a message.
fn image_input(
    message: &ConversationMessage,
    slot: ImageSlot,
    paths: Option<&fx_store::AppPaths>,
) -> Option<ImageInput> {
    match slot {
        ImageSlot::Content(index) => match message.content.get(index)? {
            MessageContent::Image { data, .. } => Some(ImageInput::inline(data)),
            MessageContent::ImageAsset(reference) => Some(ImageInput::asset(paths, reference)),
            _ => None,
        },
        ImageSlot::Mentioned(index) => mentioned_attachments(message)
            .into_iter()
            .filter(|file| file.is_image)
            .nth(index)
            .map(|file| ImageInput::Mentioned(file.path)),
    }
}

/// Act on a choice from a transcript context menu.
pub(crate) fn apply_menu_choice(owner: &str, chosen: &str, cx: &mut App) {
    if chosen == COPY_SELECTION {
        selection::copy_selection(cx);
        return;
    }
    let Some(target) = MenuTarget::parse(owner) else {
        return;
    };
    match (chosen, target.image) {
        (COPY_MESSAGE, _) => {
            copy_message(target.conversation, target.message, cx);
        }
        (COPY_IMAGE, Some(slot)) => {
            let Some(message) = drawn_message(target.conversation, target.message) else {
                return;
            };
            let paths = crate::AppHandle::get(cx).map(|handle| handle.0.read(cx).paths.clone());
            let item = image_input(&message, slot, paths.as_ref())
                .ok_or("Image unavailable")
                .and_then(|input| clipboard_image(&input));
            match item {
                Ok(item) => cx.write_to_clipboard(item),
                // A menu has no room to explain itself, and the preview above
                // it already says why the image is unavailable.
                Err(reason) => tracing::info!(reason, "an image could not be copied"),
            }
        }
        _ => {}
    }
}

// ── The transcript as text ─────────────────────────────────────────────────

/// The transcript's selectable paragraphs, read from the messages.
///
/// What a selection reaches for when its rows are not on screen. Each row's
/// paragraphs come from the same helpers its element is built from — keys from
/// [`Place::key`], shown text from `prompt_shown`, `bounded_detail` and
/// `markdown::paragraphs` — so the text copied off screen is the text the row
/// would have drawn.
struct TranscriptDocument {
    snapshot: Rc<Snapshot>,
    rows: Rows,
    heads: Rc<HashMap<Uuid, usize>>,
}

impl selection::Document for TranscriptDocument {
    fn rows(&self) -> usize {
        self.rows.len()
    }

    fn row_of(&self, key: &str) -> Option<usize> {
        if key.starts_with(STREAMING_PREFIX) {
            return self.rows.tail_index();
        }
        // A message's paragraphs are keyed by the message; a group's by its
        // own key, which names the conversation and then its first message.
        let head = match key.split_once(":work:") {
            Some((_, group)) => group,
            None => key,
        };
        let id: Uuid = head.get(..36)?.parse().ok()?;
        self.heads.get(&id).copied()
    }

    fn paragraphs(&self, row: usize) -> Vec<Paragraph> {
        let snapshot = &self.snapshot;
        match self.rows.get(row) {
            Some(RowPlan::Message(index)) => {
                message_paragraphs(&snapshot.messages[index], snapshot.id)
            }
            Some(RowPlan::Activity {
                messages,
                index,
                is_active,
            }) => {
                let messages = group_messages(snapshot, &messages);
                group_paragraphs(&group_for(snapshot.id, &messages, index, is_active))
            }
            Some(RowPlan::Tail) if !snapshot.streaming.trim().is_empty() => {
                crate::markdown::paragraphs(&snapshot.streaming, streaming_id(snapshot.id))
            }
            Some(RowPlan::Tail | RowPlan::Failure) | None => Vec::new(),
        }
    }
}

/// The paragraphs [`message_row`] makes selectable, in the order it draws them.
fn message_paragraphs(message: &ConversationMessage, conversation: Uuid) -> Vec<Paragraph> {
    let is_user = message.role == MessageRole::User;
    let mut paragraphs = Vec::new();
    for (index, block) in message.content.iter().enumerate() {
        let place = Place {
            conversation,
            message: message.id,
            index,
        };
        let (part, text): (&str, SharedString) = match block {
            MessageContent::Text { text } => {
                let text = if is_user {
                    user_prompt(text).text
                } else {
                    text
                };
                if text.trim().is_empty() {
                    continue;
                }
                if !is_user {
                    paragraphs.extend(crate::markdown::paragraphs(
                        text,
                        format!("{}:{}:md", place.message, place.index),
                    ));
                    continue;
                }
                ("text", prompt_shown(text, place).0)
            }
            MessageContent::Code { code, .. } => ("code", bounded_detail(code).into()),
            MessageContent::ToolUse { name, input, .. } => {
                if name.eq_ignore_ascii_case(QUESTION_TOOL_NAME) {
                    // A question the card can read shows no raw arguments.
                    if !parse_questions(input).is_empty() {
                        continue;
                    }
                    ("arguments", bounded_detail(input).into())
                } else if !input.is_empty() && input != "{}" {
                    ("input", bounded_detail(input).into())
                } else {
                    continue;
                }
            }
            MessageContent::ToolResult { content, .. } => {
                ("output", bounded_detail(content).into())
            }
            MessageContent::Image { .. } | MessageContent::ImageAsset(_) => continue,
        };
        paragraphs.push(Paragraph {
            key: place.key(part),
            text,
            tight: false,
        });
    }
    paragraphs
}

/// The paragraphs [`activity_group`] makes selectable: the failures among the
/// calls it shows, and nothing while it is closed.
fn group_paragraphs(group: &ActivityGroup<'_>) -> Vec<Paragraph> {
    if !is_disclosed(&group.key) {
        return Vec::new();
    }
    let hidden = group.calls.len().saturating_sub(detail_limit(&group.key));
    group
        .calls
        .iter()
        .enumerate()
        .skip(hidden)
        .filter_map(|(position, call)| match (call.is_error, call.output) {
            (true, Some(output)) => Some(Paragraph {
                key: failure_key(group, position),
                text: bounded_detail(output).into(),
                tight: false,
            }),
            _ => None,
        })
        .collect()
}

/// The selection key of one call's failure panel.
fn failure_key(group: &ActivityGroup<'_>, position: usize) -> SharedString {
    SharedString::from(format!("{}:{position}:failure", group.key))
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

/// Whether there is a live tail to show: text arriving, or a turn working with
/// nothing to report yet.
///
/// Waiting on the reader is not working, and saying otherwise would claim
/// progress that is not happening. A live activity group already says the turn
/// is working.
fn has_tail(snapshot: &Snapshot, has_active_group: bool) -> bool {
    !snapshot.streaming.trim().is_empty()
        || (snapshot.busy && !snapshot.waiting_for_user && !has_active_group)
}

/// The id the reply still streaming in is rendered under.
fn streaming_id(conversation: Uuid) -> String {
    format!("{STREAMING_PREFIX}{conversation}")
}

/// The reply still being written, or the indicator that stands in for one before
/// any of it has arrived. `None` when there is nothing live to show.
fn streaming_tail(snapshot: &Snapshot, frame: u32, has_active_group: bool) -> Option<AnyElement> {
    if !has_tail(snapshot, has_active_group) {
        return None;
    }
    let theme = global::current();
    let text = &*snapshot.streaming;

    if !text.trim().is_empty() {
        let body = Typography::style(TextRole::Body, &theme);
        return Some(
            div()
                .debug_selector(|| STREAMING_SELECTOR.to_string())
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
                                .child(crate::markdown::render(text, streaming_id(snapshot.id))),
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
                            .child(SharedString::from(snapshot.phase.label())),
                    ),
            )
            .into_any_element(),
    )
}

/// A way out of a failed turn, other than dismissing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Send the prompt again: the one that never arrived, or the last one.
    Retry,
    /// Ask the provider session the turn was running in to continue.
    Resume,
    /// Open a fresh provider session on the same thread and send again.
    Restart,
}

impl Recovery {
    fn label(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::Resume => "Resume session",
            Self::Restart => "Restart session",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Retry => Icon::ARROW_COUNTERCLOCKWISE,
            Self::Resume => Icon::ARROW_CLOCKWISE,
            Self::Restart => Icon::ARROW_CLOCKWISE_CIRCLE,
        }
    }
}

/// Failures no button here can fix: the reader has to install or configure
/// something first.
const UNRECOVERABLE_TERMS: [&str; 6] = [
    "not found",
    "install with",
    "configure it in settings",
    "failed to start",
    "quarantine",
    "could not find a usable",
];

/// Which way out a failure offers.
///
/// Ported from the original's banner, with one deliberate change: a prompt
/// the provider never accepted is retried rather than resumed. Resuming sends
/// "continue" to a session that never saw the question.
fn recovery_for(error: &str, facts: RecoveryFacts) -> Option<Recovery> {
    let error = error.to_lowercase();
    let mentions_session = ["session", "resume", "thread", "conversation"]
        .iter()
        .any(|term| error.contains(term));
    let is_stale = [
        "not found",
        "not ready",
        "invalid",
        "expired",
        "no such",
        "does not exist",
        "unknown",
    ]
    .iter()
    .any(|term| error.contains(term));
    // Checked before recoverability: "session not found" is exactly the
    // failure a fresh session fixes, even though "not found" alone is not.
    if mentions_session && is_stale && facts.session {
        return Some(Recovery::Restart);
    }
    if UNRECOVERABLE_TERMS.iter().any(|term| error.contains(term)) {
        return None;
    }
    if facts.unsent {
        Some(Recovery::Retry)
    } else if facts.session {
        Some(Recovery::Resume)
    } else if facts.prompt {
        Some(Recovery::Retry)
    } else {
        None
    }
}

fn error_banner(message: &str, snapshot: &Snapshot) -> AnyElement {
    let theme = global::current();
    let agent = snapshot.agent_id;
    let mut actions = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(px_len(Spacing::SM));
    if let Some(recovery) = recovery_for(message, snapshot.recovery) {
        actions = actions.child(
            div()
                .debug_selector(move || format!("transcript.recovery:{}", recovery.label()))
                .child(compact_action_button(
                    recovery.label(),
                    Some(recovery.icon()),
                    move |_, _, cx| crate::recover_turn(agent, Some(recovery), cx),
                )),
        );
    }
    actions = actions.child(
        div()
            .debug_selector(|| "transcript.recovery:Dismiss".to_string())
            .child(compact_action_button("Dismiss", None, move |_, _, cx| {
                crate::recover_turn(agent, None, cx)
            })),
    );
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
                .gap(px_len(Spacing::SM))
                .child(
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
                )
                .child(actions),
        ))
        .into_any_element()
}

// ── The activity group ─────────────────────────────────────────────────────

/// A collapsible group of tool traffic.
fn activity_group(group: &ActivityGroup<'_>, snapshot: &Snapshot, frame: u32) -> AnyElement {
    let theme = global::current();
    // Progress stays compact until the reader asks for details. Streaming new
    // calls must never override a reader's decision to close the disclosure.
    let expanded = is_disclosed(&group.key);

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
        .debug_selector(|| ACTIVITY_HEADER_SELECTOR.into())
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
                .child(SharedString::from(group_title(
                    group,
                    snapshot.phase.label(),
                ))),
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
            .debug_selector(|| ACTIVITY_DETAILS_SELECTOR.into())
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
        let hidden = group.calls.len().saturating_sub(detail_limit(&group.key));
        if hidden > 0 {
            let key = group.key.clone();
            details = details.child(
                div()
                    .debug_selector(|| EARLIER_ACTIONS_SELECTOR.into())
                    .child(button(
                        SharedString::from(format!(
                            "Show {} earlier {}",
                            hidden.min(ACTIVITY_PAGE_SIZE),
                            if hidden == 1 { "action" } else { "actions" }
                        )),
                        ButtonStyle::Ghost,
                        true,
                        move |_, _, cx| {
                            reveal_earlier_actions(&key);
                            cx.refresh_windows();
                        },
                    )),
            );
        }
        for (position, call) in group.calls.iter().enumerate().skip(hidden) {
            let key = failure_key(group, position);
            details = details.child(tool_row(call, group.is_active, snapshot, frame, key));
        }
        column = column.child(details);
    }

    column.into_any_element()
}

/// One call inside a group, with its state at the trailing edge.
fn tool_row(
    call: &ToolCall<'_>,
    group_is_active: bool,
    snapshot: &Snapshot,
    frame: u32,
    failure_key: SharedString,
) -> AnyElement {
    let theme = global::current();
    // A successful result that said nothing is never recorded as a row, so the
    // transcript cannot tell a finished call from a running one on its own. The
    // engine's completed-id memory is what closes that gap.
    let finished = call.result_recorded || snapshot.has_completed_tool(&call.id);
    let running = !finished && group_is_active;

    let status: AnyElement = if call.is_error {
        Icon::XMARK_CIRCLE_FILL.render(IconSize::Small, Colors::error(&theme), &theme)
    } else if running {
        spinner_arc(Colors::accent(&theme), spin_degrees(frame)).into_any_element()
    } else {
        Icon::CHECKMARK.render(IconSize::Micro, Colors::success(&theme), &theme)
    };

    let title = div()
        .debug_selector({
            let id = call.id.clone();
            move || format!("transcript.activity-call:{id}")
        })
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
        column = column.child(mono_panel(
            "Failure",
            output,
            BlockTone::Failure,
            failure_key,
            false,
        ));
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

fn group_title(group: &ActivityGroup<'_>, phase_label: &str) -> String {
    let mut title = match (group.is_active, group.summary.seconds) {
        (true, _) => format!("{phase_label}…"),
        (false, Some(seconds)) => format!("Worked for {}", duration_label(seconds)),
        (false, None) => "Worked".to_string(),
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
///
/// `key` makes the body selectable. `copyable` adds the copy button, which
/// copies `body` whole: the panel shows a bounded excerpt, and a copy of the
/// excerpt would silently be a different program.
fn mono_panel(
    label: &str,
    body: &str,
    tone: BlockTone,
    key: SharedString,
    copyable: bool,
) -> AnyElement {
    let theme = global::current();
    let shown = SharedString::from(bounded_detail(body));
    let panel_id = SharedString::from(format!("panel:{key}"));
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
                .gap(px_len(Spacing::SM))
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
                )
                .children(
                    copyable.then(|| crate::markdown::copy_code_button(body.to_string().into())),
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
                .child(SelectableText::new(
                    key,
                    shown.clone(),
                    StyledText::new(shown),
                )),
        )
        // An id of its own, so two code blocks in one message do not share
        // their copy buttons' interaction state.
        .id(panel_id)
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
fn question_card(input: &str, place: Place) -> AnyElement {
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
            .child(mono_panel(
                "Arguments",
                input,
                BlockTone::Standard,
                place.key("arguments"),
                false,
            ))
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
fn pending_prompts(snapshot: &Snapshot) -> AnyElement {
    let approvals = &snapshot.approvals;
    let questions = &snapshot.questions;
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
            SharedString::from(format!("approval:{}", approval.id)),
            false,
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

    thread_local! {
        /// How many row elements this thread has built.
        pub(super) static ROWS_BUILT: Cell<usize> = const { Cell::new(0) };
        /// Every splice this thread has told a list about.
        pub(super) static SPLICES: RefCell<Vec<(Range<usize>, usize)>> =
            const { RefCell::new(Vec::new()) };
    }

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
        assert!(matches!(rows[0], Row::Message(prompt) if prompt.role == MessageRole::User));
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
        assert!(group_title(group, "Working").contains("4 actions"));
        assert!(group_title(group, "Working").contains("2 edits"));
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
        assert_eq!(
            group_title(only_group(&rows), "Working"),
            "Working… · 1 action"
        );
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

        prune_disclosures(conversation, 1, 0, || HashSet::from([live.clone()]));
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
        prune_disclosures(conversation, 7, 0, HashSet::new);
        toggle_disclosure(&stale);
        // Same revision: the pass is skipped, so the entry survives.
        prune_disclosures(conversation, 7, 0, HashSet::new);
        assert!(is_disclosed(&stale));
        // Next revision: the group really is gone.
        prune_disclosures(conversation, 8, 0, HashSet::new);
        assert!(!is_disclosed(&stale));
    }

    /// The key a group at `index` of a turn would be stored under.
    fn group_key(conversation: Uuid, index: usize) -> String {
        let first = ConversationMessage::text(MessageRole::Assistant, "x");
        activity_key(conversation, &[&first], index)
    }

    #[test]
    fn one_list_state_survives_across_passes_for_a_thread() {
        // Two passes must land on the same list, or the reader's place and
        // every measured height are thrown away on every repaint.
        let conversation = Uuid::from_u128(0xc0de);
        let first = list_state(conversation);
        let second = list_state(conversation);
        first.splice(0..0, 3);
        first.scroll_to(ListOffset {
            item_ix: 1,
            offset_in_item: px(42.0),
        });
        let top = second.logical_scroll_top();
        assert_eq!(
            (top.item_ix, top.offset_in_item),
            (1, px(42.0)),
            "two passes must land on the same list"
        );
        assert_eq!(
            list_state(Uuid::from_u128(0xc0df)).item_count(),
            0,
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

    fn working_calls(count: usize) -> Conversation {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.apply(ProviderEvent::Lifecycle(Lifecycle::TurnStarted));
        for index in 0..count {
            conversation.apply(ProviderEvent::ToolUse {
                id: format!("tool-{index}"),
                name: "commandExecution".into(),
                input: format!(r#"{{"command":"echo {index}"}}"#),
            });
            conversation.apply(ProviderEvent::ToolResult {
                id: format!("tool-{index}"),
                content: "Completed".into(),
                is_error: false,
            });
        }
        conversation
    }

    #[test]
    fn live_activity_starts_compact_and_can_be_closed_while_work_continues() {
        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Harness {
            conversation: working_calls(12),
        });
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(900.0), px(600.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        let collapsed = visual.debug_bounds(COLUMN_SELECTOR).unwrap().size.height;
        assert!(
            collapsed < px(120.0),
            "twelve actions should occupy one summary row"
        );
        assert!(visual.debug_bounds(ACTIVITY_DETAILS_SELECTOR).is_none());
        let header = visual
            .debug_bounds(ACTIVITY_HEADER_SELECTOR)
            .unwrap()
            .center();
        visual.simulate_mouse_down(header, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(header, gpui::MouseButton::Left, Default::default());
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(
            visual
                .debug_bounds(ACTIVITY_DETAILS_SELECTOR)
                .unwrap()
                .size
                .height
                > px(200.0)
        );
        let header = visual
            .debug_bounds(ACTIVITY_HEADER_SELECTOR)
            .unwrap()
            .center();
        visual.simulate_mouse_down(header, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(header, gpui::MouseButton::Left, Default::default());
        window
            .update(&mut test, |view, _, cx| {
                view.conversation.apply(ProviderEvent::ToolUse {
                    id: "another-call".into(),
                    name: "Read".into(),
                    input: "{}".into(),
                });
                assert!(view.conversation.is_busy());
                cx.notify();
            })
            .unwrap();
        test.refresh().unwrap();
        test.update(|_| {});
        assert_eq!(
            visual.debug_bounds(COLUMN_SELECTOR).unwrap().size.height,
            collapsed
        );
    }

    #[test]
    fn expanded_activity_pages_earlier_calls_instead_of_rendering_the_entire_run() {
        let conversation = working_calls(45);
        let messages: Vec<_> = conversation.messages().collect();
        let rows = build_rows(&messages, true, conversation.id);
        toggle_disclosure(&only_group(&rows).key);
        let mut test = TestAppContext::single();
        let window = test.add_window(|_, _| Harness { conversation });
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(900.0), px(1600.0)));
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(
            visual
                .debug_bounds("transcript.activity-call:tool-24")
                .is_none()
        );
        assert!(
            visual
                .debug_bounds("transcript.activity-call:tool-25")
                .is_some()
        );
        assert!(
            visual
                .debug_bounds("transcript.activity-call:tool-44")
                .is_some()
        );
        let earlier = visual
            .debug_bounds(EARLIER_ACTIONS_SELECTOR)
            .unwrap()
            .center();
        visual.simulate_mouse_down(earlier, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(earlier, gpui::MouseButton::Left, Default::default());
        test.refresh().unwrap();
        test.update(|_| {});
        assert!(
            visual
                .debug_bounds("transcript.activity-call:tool-4")
                .is_none()
        );
        assert!(
            visual
                .debug_bounds("transcript.activity-call:tool-5")
                .is_some()
        );
    }

    #[test]
    fn activity_progress_replaces_only_the_empty_working_indicator() {
        let mut conversation = working_calls(12);
        assert!(streaming_tail(&snapshot(&conversation), 0, true).is_none());
        assert!(streaming_tail(&snapshot(&conversation), 0, false).is_some());
        conversation.apply(ProviderEvent::TextDelta("Here is what I found.".into()));
        assert!(streaming_tail(&snapshot(&conversation), 0, true).is_some());
    }

    #[test]
    fn compact_activity_preserves_stopping_compaction_and_approval_status() {
        use fx_agent::conversation::state::Phase;
        for phase in [
            Phase::Compacting,
            Phase::WaitingForApproval,
            Phase::Cancelling,
        ] {
            let mut conversation = working_calls(12);
            conversation.set_phase(phase);
            let messages: Vec<_> = conversation.messages().collect();
            let rows = build_rows(&messages, conversation.is_busy(), conversation.id);
            assert_eq!(
                group_title(only_group(&rows), conversation.phase().label()),
                format!("{}… · 12 actions", phase.label())
            );
            assert!(streaming_tail(&snapshot(&conversation), 0, true).is_none());
        }
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
            // A prompt long enough to wrap fills its measure. Anything narrower
            // is a bubble that sized itself to its longest word.
            assert_eq!(
                user.size.width,
                px(Layout::USER_MESSAGE_MAX_WIDTH).min(assistant.size.width),
                "the prompt bubble lost its reading measure at {width}"
            );
            assert!(assistant.origin.y >= user.bottom() + px(Spacing::XXL));
        }
    }

    #[test]
    fn a_populated_transcript_grows_and_scrolls_instead_of_compressing_its_rows() {
        let build = || {
            let mut conversation = Conversation::new(Uuid::new_v4());
            for index in 0..30 {
                conversation.append(text(
                    MessageRole::Assistant,
                    &format!("Message {index} stays at its natural reading height."),
                ));
            }
            conversation
        };
        let conversation = build();
        let id = conversation.id;
        let measured = measured_transcript(
            conversation,
            Layout::MINIMUM_CONVERSATION_WIDTH,
            400.0,
            &[SCROLL_ID, COLUMN_SELECTOR],
        );
        let (viewport, row) = (measured[0], measured[1]);
        // The same row with all the room it could want.
        let natural = measured_transcript(
            build(),
            Layout::MINIMUM_CONVERSATION_WIDTH,
            2_400.0,
            &[COLUMN_SELECTOR],
        )[0];
        let list = list_state(id);
        assert_eq!(viewport.size.height, px(400.0));
        // Every row keeps its natural height, so the thirty of them stand
        // taller than two viewports rather than being squeezed into one.
        assert_eq!(row.size.height, natural.size.height);
        assert!(row.size.height * 30.0 > viewport.size.height * 2.0);
        assert!(list.max_offset_for_scrollbar().height > viewport.size.height);
        assert!(
            list.logical_scroll_top().item_ix >= list.item_count(),
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
    fn unavailable_images_paint_as_compact_fallbacks() {
        let mut conversation = Conversation::new(Uuid::nil());
        conversation.append(message(
            MessageRole::User,
            vec![
                MessageContent::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
                MessageContent::ImageAsset(fx_core::ConversationImageAssetReference {
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
        // The retention cap is 250. This is the shape that stalls a layout if
        // anything can, and the one the list exists to keep cheap.
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

    // ── The snapshot ────────────────────────────────────────────────────────

    /// Bytes allocated on this thread, for asserting that a pass copies nothing.
    ///
    /// Per thread because tests run concurrently in one process, and another
    /// test's allocations must not land in this one's count.
    mod allocations {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        pub struct Counting;

        thread_local! {
            // Const-initialized and without a destructor, so touching it from
            // inside the allocator can never allocate in turn.
            static BYTES: Cell<usize> = const { Cell::new(0) };
        }

        fn count(bytes: usize) {
            let _ = BYTES.try_with(|total| total.set(total.get().wrapping_add(bytes)));
        }

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                count(layout.size());
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                count(layout.size());
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                count(new_size);
                unsafe { System.realloc(ptr, layout, new_size) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { System.dealloc(ptr, layout) }
            }
        }

        /// Run `f`, and report how many bytes it asked this thread's allocator for.
        pub fn during<R>(f: impl FnOnce() -> R) -> (R, usize) {
            let before = BYTES.with(Cell::get);
            let result = f();
            (result, BYTES.with(Cell::get).wrapping_sub(before))
        }
    }

    #[global_allocator]
    static ALLOCATOR: allocations::Counting = allocations::Counting;

    /// A turn heavy enough that copying it would be unmistakable: a 4 MB
    /// screenshot and a 1 MB tool output.
    fn heavy_conversation() -> Conversation {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.append(text(MessageRole::User, "what is on screen?"));
        conversation.apply(ProviderEvent::Lifecycle(Lifecycle::TurnStarted));
        conversation.append(message(
            MessageRole::Assistant,
            vec![MessageContent::Image {
                data: vec![7; 4 << 20],
                mime_type: "image/png".into(),
            }],
        ));
        conversation.apply(ProviderEvent::ToolUse {
            id: "read".into(),
            name: "Read".into(),
            input: r#"{"file_path":"/tmp/big.log"}"#.into(),
        });
        conversation.apply(ProviderEvent::ToolResult {
            id: "read".into(),
            content: "x".repeat(1 << 20),
            is_error: false,
        });
        conversation
    }

    #[test]
    fn a_repaint_shares_the_previous_pass_messages_instead_of_copying_them() {
        let conversation = heavy_conversation();
        let first = snapshot(&conversation);
        let (second, bytes) = allocations::during(|| snapshot(&conversation));
        assert!(
            Rc::ptr_eq(&first.messages, &second.messages),
            "an unchanged transcript was rebuilt"
        );
        assert!(
            bytes < 64 * 1024,
            "a repaint snapshot allocated {bytes} bytes"
        );
    }

    #[test]
    fn grouping_a_turn_borrows_it_rather_than_copying_it() {
        // `push_turn` used to clone every message of the turn for the grouping
        // policy — the screenshot and the tool output included — on every pass.
        let conversation = heavy_conversation();
        let snapshot = snapshot(&conversation);
        let messages: Vec<&ConversationMessage> =
            snapshot.messages.iter().map(|message| &**message).collect();
        let (rows, bytes) = allocations::during(|| build_rows(&messages, true, snapshot.id));
        assert!(rows.iter().any(|row| matches!(row, Row::Activity(_))));
        assert!(bytes < 64 * 1024, "grouping allocated {bytes} bytes");
    }

    #[test]
    fn a_new_message_is_the_only_one_copied_into_the_next_snapshot() {
        let mut conversation = heavy_conversation();
        let before = snapshot(&conversation);
        conversation.append(text(MessageRole::Assistant, "A login screen."));
        let (after, bytes) = allocations::during(|| snapshot(&conversation));
        assert_eq!(after.messages.len(), before.messages.len() + 1);
        for (old, new) in before.messages.iter().zip(after.messages.iter()) {
            assert!(Arc::ptr_eq(old, new), "an unchanged message was copied");
        }
        assert!(bytes < 64 * 1024, "the snapshot allocated {bytes} bytes");
    }

    #[test]
    fn a_message_rewritten_under_the_same_id_is_read_again() {
        // A provider refresh can return a message under the id it already had,
        // with more in it. The shared copy must not hide the new text.
        let mut conversation = Conversation::new(Uuid::new_v4());
        let mut original = text(MessageRole::Assistant, "partial");
        conversation.append(original.clone());
        let before = snapshot(&conversation);
        original.content = vec![MessageContent::Text {
            text: "partial, then finished".into(),
        }];
        conversation.replace_messages(vec![original]);
        let after = snapshot(&conversation);
        assert!(!Arc::ptr_eq(&before.messages[0], &after.messages[0]));
        assert_eq!(after.messages[0].text_content(), "partial, then finished");
    }

    #[test]
    fn rendering_a_guarded_conversation_releases_its_lock_before_drawing() {
        struct Guarded {
            conversation: Rc<Mutex<Conversation>>,
            free_while_drawing: Rc<std::cell::Cell<Option<bool>>>,
        }
        impl Render for Guarded {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let element = view_guarded(&self.conversation, window, cx);
                // The element tree is built by now. A lock still held here would
                // also be held across everything that follows in the frame.
                self.free_while_drawing
                    .set(Some(self.conversation.try_lock().is_ok()));
                element
            }
        }
        let conversation = Rc::new(Mutex::new(heavy_conversation()));
        let free = Rc::new(std::cell::Cell::new(None));
        let mut test = TestAppContext::single();
        let (for_view, for_check) = (conversation.clone(), free.clone());
        test.add_window(move |_, _| Guarded {
            conversation: for_view,
            free_while_drawing: for_check,
        });
        test.refresh().unwrap();
        assert_eq!(free.get(), Some(true));
    }

    // ── Memory ──────────────────────────────────────────────────────────────

    #[test]
    fn the_viewport_focus_handle_is_made_once_per_thread() {
        let test = TestAppContext::single();
        let conversation = Uuid::new_v4();
        let made = std::cell::Cell::new(0);
        test.update(|cx| {
            let first = viewport_focus(conversation, || {
                made.set(made.get() + 1);
                cx.focus_handle()
            });
            let second = viewport_focus(conversation, || {
                made.set(made.get() + 1);
                cx.focus_handle()
            });
            assert_eq!(first, second, "a repaint replaced the focused handle");
        });
        assert_eq!(made.get(), 1, "a handle was made and thrown away");
    }

    #[test]
    fn deleting_a_thread_forgets_everything_the_view_kept_for_it() {
        let deleted = Uuid::from_u128(0xdead);
        let kept = Uuid::from_u128(0xbeef);
        let gone = snapshot(&Conversation::new(deleted));
        let stays = snapshot(&Conversation::new(kept));
        let gone_group = group_key(gone.id, 0);
        let kept_group = group_key(stays.id, 0);
        toggle_disclosure(&gone_group);
        toggle_disclosure(&kept_group);
        reveal_earlier_actions(&gone_group);

        forget_thread(deleted);

        MEMORY.with(|memory| {
            let memory = memory.borrow();
            assert!(!memory.threads.contains_key(&gone.id));
            assert!(memory.threads.contains_key(&stays.id));
            assert!(!memory.detail_limits.contains_key(&gone_group));
        });
        assert!(!is_disclosed(&gone_group));
        assert!(
            is_disclosed(&kept_group),
            "another thread's memory went too"
        );
    }

    #[test]
    fn a_pass_sweeps_memory_for_threads_the_state_no_longer_holds() {
        let temp = tempfile::tempdir().unwrap();
        let paths = fx_store::AppPaths::at(temp.path().join("state"));
        let test = TestAppContext::single();
        test.update(|cx| {
            let state = cx.new(|cx| crate::AppState::restore(paths, cx));
            crate::AppHandle::install(state.clone(), cx);
            let live = Conversation::new(Uuid::new_v4());
            let live_agent = live.agent_id;
            state.update(cx, |state, _| {
                state
                    .conversations
                    .insert(live_agent, std::sync::Arc::new(Mutex::new(live.clone())));
            });
            // A thread removed without the view hearing about it, and an
            // older conversation object for the thread still on screen.
            let orphan = snapshot(&Conversation::new(Uuid::new_v4()));
            let mut replaced = Conversation::new(live_agent);
            replaced.id = Uuid::new_v4();
            let replaced = snapshot(&replaced);
            let shown = snapshot(&live);

            sweep_memory(&shown, cx);

            MEMORY.with(|memory| {
                let memory = memory.borrow();
                assert!(memory.threads.contains_key(&shown.id));
                assert!(!memory.threads.contains_key(&orphan.id));
                assert!(!memory.threads.contains_key(&replaced.id));
            });
        });
    }

    // ── Copying ─────────────────────────────────────────────────────────────

    /// A window that renders the transcript, kept open for driving.
    fn open(
        conversation: Conversation,
        width: f32,
        height: f32,
    ) -> (TestAppContext, VisualTestContext) {
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_window, _cx| Harness { conversation });
        let visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(width), px(height)));
        test.refresh().unwrap();
        (test, visual)
    }

    fn clipboard_text(test: &TestAppContext) -> Option<String> {
        test.read_from_clipboard().and_then(|item| item.text())
    }

    #[test]
    fn a_drag_across_two_paragraphs_copies_both_with_cmd_c() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let answer = text(
            MessageRole::Assistant,
            "The first paragraph says hello.\n\nThe second one says goodbye.",
        );
        let id = answer.id;
        conversation.append(answer);
        let (mut test, mut visual) = open(conversation, 700.0, 600.0);
        let first = crate::selection::tests::painted_bounds(&format!("{id}:0:md:0"))
            .expect("the first paragraph is selectable");
        let second = crate::selection::tests::painted_bounds(&format!("{id}:0:md:1"))
            .expect("the second paragraph is selectable");

        let start = gpui::point(first.origin.x + px(1.0), first.center().y);
        let end = gpui::point(second.right() - px(1.0), second.center().y);
        visual.simulate_mouse_down(start, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_move(end, Some(gpui::MouseButton::Left), Default::default());
        visual.simulate_mouse_up(end, gpui::MouseButton::Left, Default::default());
        test.refresh().unwrap();
        visual.simulate_keystrokes("cmd-c");

        assert_eq!(
            clipboard_text(&test).as_deref(),
            Some("The first paragraph says hello.\n\nThe second one says goodbye.")
        );
        assert!(
            message_menu(&text(MessageRole::Assistant, "x")).is_some(),
            "a message always offers to copy itself"
        );
        let menu = message_menu(&ConversationMessage {
            id,
            ..text(MessageRole::Assistant, "x")
        })
        .unwrap();
        assert_eq!(
            menu.sections()[0].items[0].id,
            COPY_SELECTION,
            "a menu over the selection leads with copying it"
        );
    }

    #[test]
    fn a_press_without_a_drag_leaves_nothing_for_cmd_c() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let answer = text(MessageRole::Assistant, "Some text to click on.");
        let id = answer.id;
        conversation.append(answer);
        let (test, mut visual) = open(conversation, 700.0, 600.0);
        let bounds = crate::selection::tests::painted_bounds(&format!("{id}:0:md:0")).unwrap();
        let at = bounds.center();
        visual.simulate_mouse_down(at, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(at, gpui::MouseButton::Left, Default::default());
        visual.simulate_keystrokes("cmd-c");
        assert_eq!(clipboard_text(&test), None);
    }

    #[test]
    fn a_whole_message_copies_as_the_reader_saw_it() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let id = conversation.id;
        let prompt = text(MessageRole::User, "Please fix the build.");
        let prompt_id = prompt.id;
        conversation.append(prompt);
        let answer = message(
            MessageRole::Assistant,
            vec![
                MessageContent::Text {
                    text: "Run **this**:".into(),
                },
                MessageContent::Code {
                    language: "sh".into(),
                    code: "cargo build".into(),
                },
                MessageContent::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
            ],
        );
        let answer_id = answer.id;
        conversation.append(answer);
        let (test, _visual) = open(conversation, 700.0, 600.0);

        test.update(|cx| apply_menu_choice(&menu_owner(id, answer_id, None), COPY_MESSAGE, cx));
        assert_eq!(
            clipboard_text(&test).as_deref(),
            Some("Run **this**:\n\ncargo build"),
            "prose keeps its Markdown, code is copied whole, images are skipped"
        );
        test.update(|cx| apply_menu_choice(&menu_owner(id, prompt_id, None), COPY_MESSAGE, cx));
        assert_eq!(
            clipboard_text(&test).as_deref(),
            Some("Please fix the build.")
        );
    }

    #[test]
    fn a_prompts_hover_button_copies_it_and_says_so() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        let id = conversation.id;
        let prompt = text(MessageRole::User, "Copy me, please.");
        let prompt_id = prompt.id;
        conversation.append(prompt);
        let (test, mut visual) = open(conversation, 700.0, 600.0);
        let button = visual
            .debug_bounds(MESSAGE_COPY_SELECTOR)
            .expect("a prompt offers a copy button")
            .center();
        visual.simulate_mouse_move(button, None, Default::default());
        visual.simulate_mouse_down(button, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(button, gpui::MouseButton::Left, Default::default());
        assert_eq!(clipboard_text(&test).as_deref(), Some("Copy me, please."));
        assert!(
            is_copied(id, prompt_id),
            "the button never said it had copied"
        );
        // Leaving the row puts the label back.
        visual.simulate_mouse_move(gpui::point(px(1.0), px(590.0)), None, Default::default());
        assert!(!is_copied(id, prompt_id));
    }

    #[test]
    fn an_image_copies_its_original_bytes() {
        let mut png = Vec::new();
        image::ImageBuffer::from_pixel(4, 3, image::Rgba([10u8, 20, 30, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut conversation = Conversation::new(Uuid::new_v4());
        let id = conversation.id;
        let shot = message(
            MessageRole::User,
            vec![MessageContent::Image {
                data: png.clone(),
                mime_type: "image/png".into(),
            }],
        );
        let shot_id = shot.id;
        conversation.append(shot);
        let (test, _visual) = open(conversation, 700.0, 600.0);
        test.update(|cx| {
            apply_menu_choice(
                &menu_owner(id, shot_id, Some(ImageSlot::Content(0))),
                COPY_IMAGE,
                cx,
            )
        });
        let copied = test.read_from_clipboard().expect("the image was copied");
        match copied.entries() {
            [gpui::ClipboardEntry::Image(image)] => {
                assert_eq!(image.format, gpui::ImageFormat::Png);
                assert_eq!(image.bytes, png);
            }
            other => panic!("expected one image, got {other:?}"),
        }
    }

    #[test]
    fn menu_owners_round_trip() {
        let (conversation, message) = (Uuid::new_v4(), Uuid::new_v4());
        for image in [
            None,
            Some(ImageSlot::Content(3)),
            Some(ImageSlot::Mentioned(1)),
        ] {
            assert_eq!(
                MenuTarget::parse(&menu_owner(conversation, message, image)),
                Some(MenuTarget {
                    conversation,
                    message,
                    image
                })
            );
        }
        assert_eq!(MenuTarget::parse("thread:whatever"), None);
        assert_eq!(
            MenuTarget::parse(&format!(
                "{MENU_OWNER_PREFIX}{conversation}:{message}:odd:1"
            )),
            None
        );
    }

    // ── Virtualization ──────────────────────────────────────────────────────

    /// A long thread of prompts and Markdown answers, one paragraph each.
    fn long_conversation(messages: usize) -> Conversation {
        let mut conversation = Conversation::new(Uuid::new_v4());
        for index in 0..messages {
            conversation.append(if index.is_multiple_of(2) {
                text(MessageRole::User, &format!("Question {index}"))
            } else {
                text(
                    MessageRole::Assistant,
                    &format!("Answer {index} has **bold** words and enough of them to wrap."),
                )
            });
        }
        conversation
    }

    /// What each message of [`long_conversation`] shows, in order.
    fn long_conversation_text(messages: usize) -> Vec<String> {
        (0..messages)
            .map(|index| {
                if index.is_multiple_of(2) {
                    format!("Question {index}")
                } else {
                    format!("Answer {index} has bold words and enough of them to wrap.")
                }
            })
            .collect()
    }

    /// How many rows one more frame builds.
    fn rows_built_by_a_frame(
        test: &mut TestAppContext,
        frame: impl FnOnce(&mut TestAppContext),
    ) -> usize {
        ROWS_BUILT.with(|built| built.set(0));
        frame(test);
        ROWS_BUILT.with(Cell::get)
    }

    fn redraw(test: &mut TestAppContext) {
        test.refresh().unwrap();
        test.update(|_| {});
    }

    fn scroll_wheel(visual: &mut VisualTestContext, at: gpui::Point<Pixels>, up_by: f32) {
        visual.simulate_event(gpui::ScrollWheelEvent {
            position: at,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(up_by))),
            ..Default::default()
        });
    }

    fn is_pinned(conversation: Uuid) -> bool {
        let list = list_state(conversation);
        list.logical_scroll_top().item_ix >= list.item_count()
    }

    #[test]
    fn a_frame_builds_only_the_rows_in_view() {
        // Every row of the conversation used to be built, laid out and painted
        // on every frame, so a caret blink cost as much as the whole thread.
        let conversation = long_conversation(MAX_RETAINED_MESSAGES);
        let (mut test, visual) = open(conversation, 900.0, 800.0);
        let repaint = rows_built_by_a_frame(&mut test, redraw);
        assert!(
            (1..=24).contains(&repaint),
            "a repaint built {repaint} of {MAX_RETAINED_MESSAGES} rows"
        );
        // Each step of the sidebar's slide changes the width, which forgets
        // every height. Only the rows in view and their overdraw are rebuilt.
        for step in 1..=5 {
            let width = 900.0 - step as f32 * 30.0;
            let resized = rows_built_by_a_frame(&mut test, |test| {
                visual.simulate_resize(size(px(width), px(800.0)));
                test.update(|_| {});
            });
            assert!(
                (1..=48).contains(&resized),
                "a resize to {width} built {resized} of {MAX_RETAINED_MESSAGES} rows"
            );
        }
    }

    #[test]
    fn a_streamed_delta_re_measures_only_the_tail_and_a_new_message_only_itself() {
        let mut conversation = long_conversation(40);
        let id = conversation.id;
        conversation.apply(ProviderEvent::Lifecycle(Lifecycle::TurnStarted));
        conversation.apply(ProviderEvent::TextDelta("Streaming".into()));
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_, _| Harness { conversation });
        let visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(700.0), px(500.0)));
        redraw(&mut test);
        let rows = list_state(id).item_count();
        assert_eq!(rows, 41, "forty messages and the reply streaming in");

        SPLICES.with(|splices| splices.borrow_mut().clear());
        window
            .update(&mut test, |view, _, cx| {
                view.conversation
                    .apply(ProviderEvent::TextDelta(" a little more".into()));
                cx.notify();
            })
            .unwrap();
        redraw(&mut test);
        assert_eq!(
            SPLICES.with(|splices| splices.borrow().clone()),
            vec![(40..41, 1)],
            "a delta re-measured more than the tail"
        );

        SPLICES.with(|splices| splices.borrow_mut().clear());
        window
            .update(&mut test, |view, _, cx| {
                view.conversation.apply(ProviderEvent::Done {
                    stop_reason: "end_turn".into(),
                });
                view.conversation
                    .append(text(MessageRole::User, "And another thing"));
                cx.notify();
            })
            .unwrap();
        redraw(&mut test);
        assert_eq!(
            SPLICES.with(|splices| splices.borrow().clone()),
            vec![(40..41, 2)],
            "the committed reply and the new prompt replace the tail, and nothing before it"
        );
        assert_eq!(list_state(id).item_count(), 42);
    }

    #[test]
    fn a_new_text_size_re_measures_every_row_and_keeps_the_readers_place() {
        // Nothing about the conversation changes, but every height does.
        let conversation = long_conversation(60);
        let id = conversation.id;
        let (mut test, mut visual) = open(conversation, 700.0, 500.0);
        let middle = visual.debug_bounds(SCROLL_ID).unwrap().center();
        scroll_wheel(&mut visual, middle, 300.0);
        redraw(&mut test);
        let held = list_state(id).logical_scroll_top();
        assert!(!is_pinned(id));

        SPLICES.with(|splices| splices.borrow_mut().clear());
        let _theme = fx_design::theme::ThemeGuard::install(fx_design::theme::Theme {
            text_size: TextSizePreset::Large,
            ..global::current()
        });
        redraw(&mut test);
        assert_eq!(
            SPLICES.with(|splices| splices.borrow().clone()),
            vec![(0..60, 60)],
            "a new text size kept heights measured at the old one"
        );
        let now = list_state(id).logical_scroll_top();
        assert!(!is_pinned(id), "re-measuring sent the reader to the bottom");
        assert_eq!(
            now.item_ix, held.item_ix,
            "re-measuring lost the reader's row"
        );
    }

    #[test]
    fn following_the_bottom_survives_streaming_and_stops_when_the_reader_scrolls_up() {
        let mut conversation = long_conversation(60);
        let id = conversation.id;
        conversation.apply(ProviderEvent::Lifecycle(Lifecycle::TurnStarted));
        conversation.apply(ProviderEvent::TextDelta("The reply begins.".into()));
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_, _| Harness { conversation });
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(700.0), px(500.0)));
        redraw(&mut test);
        let stream = |test: &mut TestAppContext, line: usize| {
            window
                .update(test, |view, _, cx| {
                    view.conversation.apply(ProviderEvent::TextDelta(format!(
                        "\n\nLine {line} of a reply long enough to grow the tail past the viewport."
                    )));
                    cx.notify();
                })
                .unwrap();
            redraw(test);
        };
        let tail_meets_the_bottom = |visual: &mut VisualTestContext| {
            let viewport = visual.debug_bounds(SCROLL_ID).unwrap();
            let tail = visual.debug_bounds(STREAMING_SELECTOR).unwrap();
            (tail.bottom() - (viewport.bottom() - px(LIST_PADDING))).abs() <= px(1.0)
        };

        assert!(is_pinned(id), "a thread opens on its newest content");
        for line in 0..40 {
            stream(&mut test, line);
        }
        assert!(is_pinned(id), "a delta unpinned the view");
        assert!(
            tail_meets_the_bottom(&mut visual),
            "the growing tail ran past the bottom"
        );

        // Scrolled up: the reader's place holds while the reply keeps growing.
        let middle = visual.debug_bounds(SCROLL_ID).unwrap().center();
        scroll_wheel(&mut visual, middle, 200.0);
        redraw(&mut test);
        assert!(!is_pinned(id), "scrolling up kept following the bottom");
        let held = list_state(id).logical_scroll_top();
        assert_eq!(
            held.item_ix, 60,
            "the reader should be partway up the reply, which every delta re-measures"
        );
        for line in 40..48 {
            stream(&mut test, line);
        }
        let now = list_state(id).logical_scroll_top();
        assert_eq!(
            (now.item_ix, now.offset_in_item),
            (held.item_ix, held.offset_in_item),
            "a delta moved a reader who had scrolled up"
        );

        // Back to the bottom, and following again.
        scroll_wheel(&mut visual, middle, -100_000.0);
        redraw(&mut test);
        assert!(is_pinned(id), "scrolling back down did not follow again");
        stream(&mut test, 48);
        assert!(tail_meets_the_bottom(&mut visual));

        // Stopping within the scroll policy's tolerance still follows.
        scroll_wheel(
            &mut visual,
            middle,
            ConversationScrollPolicy::BOTTOM_TOLERANCE / 2.0,
        );
        redraw(&mut test);
        assert!(
            is_pinned(id),
            "a reader just short of the bottom stopped following"
        );
        stream(&mut test, 49);
        assert!(tail_meets_the_bottom(&mut visual));
    }

    #[test]
    fn the_page_keys_move_the_list_and_end_follows_the_bottom_again() {
        let conversation = long_conversation(80);
        let id = conversation.id;
        let last = conversation.messages().last().unwrap().id;
        let (mut test, mut visual) = open(conversation, 700.0, 500.0);
        // A press on the text is what focuses the viewport.
        let answer = crate::selection::tests::painted_bounds(&format!("{last}:0:md:0")).unwrap();
        visual.simulate_mouse_down(answer.center(), gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(answer.center(), gpui::MouseButton::Left, Default::default());

        // From the bottom, a line up moves what is on screen by a line. The
        // list's own position there is a viewport below the screen, and a
        // step taken from it used to barely move at all.
        let line = px(Typography::body_point_size(&global::current()) * 3.0);
        visual.simulate_keystrokes("up");
        redraw(&mut test);
        assert!(!is_pinned(id), "the up arrow did not move the view");
        let moved = crate::selection::tests::painted_bounds(&format!("{last}:0:md:0"))
            .unwrap()
            .origin
            .y
            - answer.origin.y;
        assert!(
            (moved - line).abs() <= px(1.0),
            "the up arrow moved {moved:?} rather than a line of {line:?}"
        );

        // A page from there moves a row on screen by a page.
        let list = list_state(id);
        let row = list.logical_scroll_top().item_ix + 1;
        let before = list.bounds_for_item(row).unwrap().top();
        visual.simulate_keystrokes("pageup");
        redraw(&mut test);
        let page = list.viewport_bounds().size.height * 0.9;
        let moved = list.bounds_for_item(row).unwrap().top() - before;
        assert!(
            (moved - page).abs() <= px(1.0),
            "Page Up moved {moved:?} rather than a page of {page:?}"
        );

        visual.simulate_keystrokes("home");
        redraw(&mut test);
        let top = list_state(id).logical_scroll_top();
        assert_eq!((top.item_ix, top.offset_in_item), (0, px(0.0)));
        visual.simulate_keystrokes("end");
        redraw(&mut test);
        assert!(is_pinned(id), "End did not return to the bottom");
    }

    #[test]
    fn each_thread_keeps_its_own_place() {
        struct Switcher {
            conversations: [Conversation; 2],
            shown: usize,
        }
        impl Render for Switcher {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div().size_full().flex().flex_col().child(view(
                    &self.conversations[self.shown],
                    window,
                    cx,
                ))
            }
        }
        let (first, second) = (long_conversation(80), long_conversation(80));
        let (first_id, second_id) = (first.id, second.id);
        let mut test = TestAppContext::single();
        let window = test.add_window(move |_, _| Switcher {
            conversations: [first, second],
            shown: 0,
        });
        let mut visual = VisualTestContext::from_window(*window.deref(), &test);
        visual.simulate_resize(size(px(700.0), px(500.0)));
        redraw(&mut test);
        let middle = visual.debug_bounds(SCROLL_ID).unwrap().center();
        for _ in 0..4 {
            scroll_wheel(&mut visual, middle, 150.0);
            redraw(&mut test);
        }
        let held = list_state(first_id).logical_scroll_top();
        assert!(!is_pinned(first_id));

        let show = |test: &mut TestAppContext, shown: usize| {
            window
                .update(test, |view, _, cx| {
                    view.shown = shown;
                    cx.notify();
                })
                .unwrap();
            redraw(test);
        };
        show(&mut test, 1);
        assert!(
            is_pinned(second_id),
            "the other thread opened somewhere else"
        );
        show(&mut test, 0);
        let back = list_state(first_id).logical_scroll_top();
        assert_eq!(
            (back.item_ix, back.offset_in_item),
            (held.item_ix, held.offset_in_item),
            "switching away and back lost the reader's place"
        );
    }

    #[test]
    fn select_all_copies_the_rows_that_are_off_screen() {
        const MESSAGES: usize = 120;
        let conversation = long_conversation(MESSAGES);
        let first = conversation.messages().next().unwrap().id;
        let last = conversation.messages().last().unwrap().id;
        let (test, mut visual) = open(conversation, 700.0, 500.0);
        assert!(
            crate::selection::tests::painted_bounds(&format!("{first}:0:text")).is_none(),
            "the first prompt should be far off screen"
        );
        let answer = crate::selection::tests::painted_bounds(&format!("{last}:0:md:0")).unwrap();
        visual.simulate_mouse_down(answer.center(), gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(answer.center(), gpui::MouseButton::Left, Default::default());
        visual.simulate_keystrokes("cmd-a");
        visual.simulate_keystrokes("cmd-c");
        assert_eq!(
            clipboard_text(&test),
            Some(long_conversation_text(MESSAGES).join("\n\n"))
        );
    }

    #[test]
    fn a_selection_whose_anchor_scrolled_away_copies_every_row_it_spans() {
        const MESSAGES: usize = 120;
        let conversation = long_conversation(MESSAGES);
        let ids: Vec<Uuid> = conversation.messages().map(|message| message.id).collect();
        let key = |index: usize| {
            if index.is_multiple_of(2) {
                format!("{}:0:text", ids[index])
            } else {
                format!("{}:0:md:0", ids[index])
            }
        };
        let (mut test, mut visual) = open(conversation, 700.0, 500.0);

        // Anchor at the end of the last answer.
        let answer = crate::selection::tests::painted_bounds(&key(MESSAGES - 1)).unwrap();
        let end = gpui::point(answer.right() - px(1.0), answer.center().y);
        visual.simulate_mouse_down(end, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            answer.center(),
            Some(gpui::MouseButton::Left),
            Default::default(),
        );
        visual.simulate_mouse_up(answer.center(), gpui::MouseButton::Left, Default::default());

        // Scroll it well out of view, a wheel's worth at a time.
        let middle = visual.debug_bounds(SCROLL_ID).unwrap().center();
        for _ in 0..8 {
            scroll_wheel(&mut visual, middle, 250.0);
            redraw(&mut test);
        }
        assert!(
            crate::selection::tests::painted_bounds(&key(MESSAGES - 1)).is_none(),
            "the anchor is still on screen"
        );

        // Extend the selection to the start of a paragraph now on screen.
        let shown = (0..MESSAGES)
            .find(|index| {
                crate::selection::tests::painted_bounds(&key(*index))
                    .is_some_and(|bounds| !bounds.is_empty())
            })
            .expect("something is on screen");
        let target = crate::selection::tests::painted_bounds(&key(shown)).unwrap();
        let start = gpui::point(target.origin.x + px(1.0), target.center().y);
        visual.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::shift());
        visual.simulate_mouse_up(start, gpui::MouseButton::Left, gpui::Modifiers::shift());
        visual.simulate_keystrokes("cmd-c");

        assert_eq!(
            clipboard_text(&test),
            Some(long_conversation_text(MESSAGES)[shown..].join("\n\n")),
            "copying from row {shown} to the scrolled-away anchor"
        );
    }

    #[test]
    fn the_document_reads_the_same_paragraphs_the_rows_draw() {
        // What a selection copies off screen must be exactly what the row would
        // have drawn: the same keys, the same text, the same joins.
        let mut conversation = Conversation::new(Uuid::new_v4());
        let id = conversation.id;
        conversation.append(text(MessageRole::User, "A short prompt."));
        conversation.append(text(
            MessageRole::User,
            &(1..=12)
                .map(|line| format!("Pasted line {line}."))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        conversation.append(text(
            MessageRole::Assistant,
            "## Heading\n\nA paragraph with `code`.\n\n- one\n- two\n  - nested\n\n---\n\n```rust\nfn main() {}\n```\n\nDone.",
        ));
        conversation.append(message(
            MessageRole::Assistant,
            vec![
                MessageContent::Text {
                    text: "Here is the plan:".into(),
                },
                MessageContent::Code {
                    language: "sh".into(),
                    code: "cargo test".into(),
                },
                MessageContent::ToolUse {
                    id: "inline".into(),
                    name: "Read".into(),
                    input: r#"{"file_path":"/tmp/a.rs"}"#.into(),
                },
                MessageContent::ToolResult {
                    id: "inline".into(),
                    content: "fn a() {}".into(),
                    is_error: false,
                },
                MessageContent::ToolUse {
                    id: "asked".into(),
                    name: QUESTION_TOOL_NAME.into(),
                    input: r#"{"questions":[{"question":"Which?","options":[{"label":"A"}]}]}"#
                        .into(),
                },
                MessageContent::ToolUse {
                    id: "odd".into(),
                    name: QUESTION_TOOL_NAME.into(),
                    input: "not json".into(),
                },
            ],
        ));
        conversation.append(text(MessageRole::User, "Run it."));
        conversation.apply(ProviderEvent::Lifecycle(Lifecycle::TurnStarted));
        conversation.apply(ProviderEvent::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: r#"{"command":"cargo test"}"#.into(),
        });
        conversation.apply(ProviderEvent::ToolResult {
            id: "t1".into(),
            content: "error: could not compile".into(),
            is_error: true,
        });
        conversation.apply(ProviderEvent::TextDelta(
            "It failed:\n\n1. first\n2. second".into(),
        ));
        let messages: Vec<_> = conversation.messages().collect();
        let rows = build_rows(&messages, true, id);
        let group = rows
            .iter()
            .find_map(|row| match row {
                Row::Activity(group) => Some(group.key.clone()),
                Row::Message(_) => None,
            })
            .unwrap();
        toggle_disclosure(&group);

        let (_test, _visual) = open(conversation, 900.0, 4_000.0);
        let painted = crate::selection::tests::painted_paragraphs();
        let document = crate::selection::tests::document_paragraphs();
        assert!(
            painted.iter().any(|(key, ..)| key.ends_with(":failure"))
                && painted
                    .iter()
                    .any(|(key, ..)| key.starts_with(STREAMING_PREFIX))
                && painted.iter().any(|(key, ..)| key.ends_with(":arguments")),
            "the fixture no longer reaches every kind of paragraph: {painted:#?}"
        );
        assert_eq!(document, painted);
    }

    // ── Long prompts ────────────────────────────────────────────────────────

    #[test]
    fn a_prompt_collapses_past_its_character_or_line_limit() {
        assert!(!can_collapse("short"));
        assert!(!can_collapse(&"a".repeat(COLLAPSED_CHARACTER_LIMIT)));
        assert!(can_collapse(&"a".repeat(COLLAPSED_CHARACTER_LIMIT + 1)));
        assert!(!can_collapse(&["line"; COLLAPSED_LINE_LIMIT].join("\n")));
        assert!(can_collapse(&["line"; COLLAPSED_LINE_LIMIT + 1].join("\n")));
        let long = "é".repeat(COLLAPSED_PREVIEW_CHARACTERS * 2);
        assert_eq!(
            collapsed_preview(&long).chars().count(),
            COLLAPSED_PREVIEW_CHARACTERS
        );
    }

    #[test]
    fn a_long_prompt_shows_more_and_less_on_request() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.append(text(
            MessageRole::User,
            &(1..=40)
                .map(|line| format!("Line {line} of a pasted log."))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        let (mut test, mut visual) = open(conversation, 900.0, 2_000.0);
        let collapsed = visual
            .debug_bounds(PROMPT_BODY_SELECTOR)
            .unwrap()
            .size
            .height;
        assert!(
            collapsed <= px(Layout::COLLAPSED_USER_MESSAGE_HEIGHT),
            "a long prompt was not collapsed: {collapsed:?}"
        );

        let toggle = visual
            .debug_bounds(PROMPT_TOGGLE_SELECTOR)
            .unwrap()
            .center();
        visual.simulate_mouse_down(toggle, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(toggle, gpui::MouseButton::Left, Default::default());
        test.refresh().unwrap();
        let expanded = visual
            .debug_bounds(PROMPT_BODY_SELECTOR)
            .unwrap()
            .size
            .height;
        assert!(
            expanded > collapsed * 2.0,
            "Show more did not expand: {expanded:?}"
        );

        let toggle = visual
            .debug_bounds(PROMPT_TOGGLE_SELECTOR)
            .unwrap()
            .center();
        visual.simulate_mouse_down(toggle, gpui::MouseButton::Left, Default::default());
        visual.simulate_mouse_up(toggle, gpui::MouseButton::Left, Default::default());
        test.refresh().unwrap();
        assert_eq!(
            visual
                .debug_bounds(PROMPT_BODY_SELECTOR)
                .unwrap()
                .size
                .height,
            collapsed
        );
    }

    #[test]
    fn a_short_prompt_has_no_disclosure() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.append(text(MessageRole::User, "Just this."));
        let (_test, mut visual) = open(conversation, 900.0, 600.0);
        assert!(visual.debug_bounds(PROMPT_TOGGLE_SELECTOR).is_none());
    }

    // ── Recovery ────────────────────────────────────────────────────────────

    #[test]
    fn a_failure_offers_the_one_recovery_that_can_work() {
        let facts = |unsent, session, prompt| RecoveryFacts {
            unsent,
            session,
            prompt,
        };
        // A dead session is restarted, even though "not found" alone is fatal.
        assert_eq!(
            recovery_for("Codex thread not found", facts(false, true, true)),
            Some(Recovery::Restart)
        );
        // A prompt that never arrived is resent, not "continued".
        assert_eq!(
            recovery_for("provider disconnected", facts(true, true, true)),
            Some(Recovery::Retry)
        );
        assert_eq!(
            recovery_for("provider disconnected", facts(false, true, true)),
            Some(Recovery::Resume)
        );
        assert_eq!(
            recovery_for("provider disconnected", facts(false, false, true)),
            Some(Recovery::Retry)
        );
        assert_eq!(
            recovery_for("provider disconnected", facts(false, false, false)),
            None
        );
        // Nothing here installs Codex.
        assert_eq!(
            recovery_for(
                "FlowX could not find a usable Codex install.",
                facts(true, true, true)
            ),
            None
        );
    }

    #[test]
    fn a_failed_turn_shows_its_recovery_and_dismiss_buttons() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.append(text(MessageRole::User, "try this"));
        conversation.set_session_id("thread-1");
        conversation.set_error("provider disconnected");
        let (_test, mut visual) = open(conversation, 900.0, 600.0);
        assert!(
            visual
                .debug_bounds("transcript.recovery:Resume session")
                .is_some()
        );
        assert!(visual.debug_bounds("transcript.recovery:Dismiss").is_some());
        assert!(visual.debug_bounds("transcript.recovery:Retry").is_none());
    }
}
