//! The Claude provider.
//!
//! Claude Code does not speak JSON-RPC. It writes one JSON object per line and
//! narrates what it is doing, so the work here is reading a sequence and holding
//! the state that sequence implies — not correlating replies.

#[cfg(test)]
mod fake_claude;
pub mod stream;
pub mod transport;

pub use stream::{
    ClaudeStreamParser, PendingControl, control_response, is_read_only_command,
    permission_decision, refuse_control, risk_for,
};
pub use transport::{
    ClaudeTransport, Exit, Launch, StreamItem, TransportError, TurnEnd, TurnInput, TurnRequest,
};
