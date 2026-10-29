//! Git integration: running `git` and parsing what it prints.

pub mod command;
pub mod diff;
pub mod operations;
pub mod revision;
pub mod status_parser;
pub mod worktree;

pub use command::{
    CommandOutput, CommandSpec, ContentRevisionBuilder, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT,
    GIT, GitError, TRUNCATION_NOTICE, hardened_environment, run, run_lenient,
};
pub use diff::{DiffLine, DiffLineKind, DiffSection, SplitRow, SplitSide, parse_unified_diff};
pub use operations::{CommitPlan, CommitScope, PushContext, push_arguments};
pub use revision::{RevisionInputs, content_revision};
pub use status_parser::{GitStatusParser, LineCounts, Status, StatusLine};
