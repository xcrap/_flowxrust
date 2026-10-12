//! `fx-store` — persistence.
//!
//! This crate owns the on-disk format and nothing else. It knows how to read
//! and write a file; it does not know what a project is for. Keeping that split
//! means the format can change without touching the app, and the app can change
//! without touching the format.

pub mod format;
pub mod paths;
mod store;
pub mod write;

pub use format::{
    PersistedApp, PersistedBinding, PersistedConversation, PersistedPreferences, PersistedProject,
    PersistedUnsentPrompt, PersistedWorkspace, RightPanelTab, SplitContent,
};
pub use paths::{AppPaths, PathsError};
pub use store::{LoadError, SaveError, Store};
pub use write::{ReadError, WriteError, read_json, write_json_atomically};
