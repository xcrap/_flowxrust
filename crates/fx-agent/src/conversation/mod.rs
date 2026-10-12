//! The conversation engine.
//!
//! A *conversation* is one thread's state: its messages, the turn in flight,
//! and everything the composer needs to know about it. This module owns that
//! state and the streaming logic that feeds it; the provider that supplies the
//! events is somebody else's problem.

pub mod delta_buffer;
pub mod state;

pub use delta_buffer::{Cadence, DEFAULT_CADENCE, DeltaPolicy};
pub use state::{
    Conversation, MAX_RETAINED_MESSAGES, PendingApproval, PendingQuestion, Phase, QuestionOption,
    QueuedPrompt, RuntimeActivity, TRIVIAL_TOOL_RESULT, UnsentPrompt, UsageTotals,
};
