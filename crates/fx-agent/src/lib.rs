//! `fx-agent` — providers and the conversation engine.
//!
//! A provider wraps one agent runtime (Codex, Claude) behind a single interface
//! so the conversation engine does not know which it is talking to. Both speak
//! JSON over stdio, which is markedly simpler in Rust than it was in Swift:
//! framing is a line reader, and the messages are `serde_json::Value` until
//! something needs the shape.

pub mod claude;
pub mod codex;
pub mod conversation;
pub mod native;
pub mod provider;
pub mod title;

pub use conversation::{Conversation, DeltaPolicy, Phase};
pub use provider::{
    Model, Provider, ProviderError, ProviderEvent, ProviderHealth, StreamSink, ThreadIdentity,
    ThreadSummary,
};
