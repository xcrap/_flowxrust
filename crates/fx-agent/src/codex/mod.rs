//! The Codex provider: `codex app-server` over newline-delimited JSON-RPC.

#[cfg(test)]
mod fake_server;
pub mod framing;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod transport;
pub mod turn;

pub use framing::{FrameError, LineFramer};
pub use protocol::{Classified, Incoming, IncomingError, Outgoing, ProtocolError};
pub use provider::{CodexProvider, list_threads_with, thread_summary, thread_transcript};
pub use session::{
    CodexWriterConflict, TurnRequest, access_params, fallback_models, list_threads_params,
    next_cursor, parse_model_catalog, parse_notification, sandbox_policy, steer_params,
    thread_id_from_result, thread_params, threads_from_result, turn_input, turn_params,
};
pub use transport::{
    CONTROL_REQUEST_TIMEOUT, Closed, Exit, Launch, PendingRequests, ServerEvent, Transport,
    TransportError,
};
pub use turn::{ApprovalPolicy, CodexSession, TurnOutcome};
