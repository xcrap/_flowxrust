//! Pure policies.
//!
//! Every module here is a decision procedure with no IO and no rendering: given
//! inputs, return a decision. That is what makes them the cheapest and highest
//! value part of the port, and what keeps them testable without a window.

pub mod conversation_scroll;
pub mod diff_budget;
pub mod diff_disclosure;
pub mod diff_row_layout;
pub mod file_index;
pub mod image_correlation;
pub mod transcript_grouping;
pub mod transcript_presentation;

pub use conversation_scroll::{
    ConversationScrollPolicy, ConversationScrollRestoration, ScrollMetrics,
};
pub use diff_budget::{
    DEFAULT_MAXIMUM_BYTES as DEFAULT_DIFF_BUDGET_BYTES, DiffBudget,
    TRUNCATION_NOTICE as DIFF_TRUNCATION_NOTICE,
};
pub use diff_disclosure::DiffDisclosureState;
pub use diff_row_layout::{DiffRowLayout, Location};
pub use file_index::{ProjectFileIndexPolicy, SKIPPED_DIRECTORY_NAMES};
pub use image_correlation::{ConversationImageCorrelation, ImageCorrelationKey};
pub use transcript_grouping::{TranscriptActivityGroupingPolicy, TranscriptActivitySegment};
