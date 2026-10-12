//! Git integration: running `git` and parsing what it prints.

pub mod command;
pub mod status_parser;

pub use command::{
    CommandOutput, CommandSpec, ContentRevisionBuilder, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_TIMEOUT,
    GIT, GitError, TRUNCATION_NOTICE, hardened_environment, run, run_lenient,
};
pub use status_parser::{GitStatusParser, LineCounts, Status, StatusLine};
