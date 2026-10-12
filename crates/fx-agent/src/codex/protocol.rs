//! The JSON-RPC 2.0 envelope `codex app-server` speaks.
//!
//! Codex is newline-delimited JSON-RPC over stdio: every line is a complete
//! message, in either direction. Responses carry `id`; notifications do not;
//! server-initiated requests carry both `method` and `id`, and must be answered
//! or the turn hangs.
//!
//! Requests FlowX sends use two fixed ids plus a counter, matching the
//! original: `0` for `initialize` and `1` for `thread/start` or `thread/resume`,
//! so the handshake can be matched without consulting the counter.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::codex::framing::FrameError;

/// The id used for `initialize`.
pub const INITIALIZE_ID: i64 = 0;
/// The id used for the thread start or resume.
pub const THREAD_ID: i64 = 1;
/// The first dynamically allocated id.
pub const FIRST_DYNAMIC_ID: i64 = 10;

/// A message FlowX sends to the server.
///
/// Serialized by hand rather than derived. JSON-RPC 2.0 is not a tagged enum:
/// the discriminator is the *presence* of `id`, and every message carries
/// `jsonrpc`. An internally tagged derive would emit `{"type":"Request",...}`,
/// which no server would accept, so the wire shape is written out explicitly.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    /// A request expecting a response.
    Request {
        id: i64,
        method: String,
        params: Option<Value>,
    },
    /// A response to a server-initiated request.
    Response { id: Value, result: Value },
    /// An error response to a server-initiated request.
    ErrorResponse {
        id: Value,
        code: i64,
        message: String,
    },
    /// A notification, which expects nothing back.
    Notification {
        method: String,
        params: Option<Value>,
    },
}

impl Serialize for Outgoing {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("jsonrpc", "2.0")?;
        match self {
            Self::Request { id, method, params } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("method", method)?;
                if let Some(params) = params {
                    map.serialize_entry("params", params)?;
                }
            }
            Self::Response { id, result } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("result", result)?;
            }
            Self::ErrorResponse { id, code, message } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("error", &json!({ "code": code, "message": message }))?;
            }
            Self::Notification { method, params } => {
                map.serialize_entry("method", method)?;
                if let Some(params) = params {
                    map.serialize_entry("params", params)?;
                }
            }
        }
        map.end()
    }
}

impl Outgoing {
    /// Serialize to the wire form: a single JSON line, newline terminated.
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The `initialize` handshake.
    pub fn initialize(client_name: &str, client_version: &str) -> Self {
        Self::Request {
            id: INITIALIZE_ID,
            method: "initialize".into(),
            params: Some(json!({
                "clientInfo": { "name": client_name, "version": client_version },
                "capabilities": { "experimentalApi": true }
            })),
        }
    }

    /// Sent once `initialize` succeeds.
    pub fn initialized() -> Self {
        Self::Notification {
            method: "initialized".into(),
            params: Some(json!({})),
        }
    }

    /// Encode and parse back, for tests that need a `Value` of an outgoing
    /// message rather than the bytes.
    #[cfg(test)]
    fn serialize_check(message: &Outgoing) -> Vec<u8> {
        message.encode().unwrap()
    }

    /// Allocate the next request id.
    ///
    /// Pre-increments, so the first dynamic id is [`FIRST_DYNAMIC_ID`] plus one.
    pub fn next_id(counter: &mut i64) -> i64 {
        *counter += 1;
        *counter
    }
}

/// A message the server sends.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<IncomingError>,
}

/// An error the server reported.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct IncomingError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

/// What kind of message arrived.
#[derive(Debug, Clone, PartialEq)]
pub enum Classified {
    /// A response to something FlowX sent.
    Response {
        id: Value,
        payload: Result<Value, IncomingError>,
    },
    /// A one-way server event.
    Notification { method: String, params: Value },
    /// A server-initiated request, which FlowX must answer.
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// A message that is none of the above, which is ignored rather than
    /// treated as fatal.
    Unknown(Value),
}

impl Incoming {
    /// Sort a message into the kind it is.
    ///
    /// `method` plus `id` is a request; `method` alone is a notification; an
    /// `id` alone is a response. Anything missing both is unknown.
    pub fn classify(self) -> Classified {
        match (self.method, self.id) {
            (Some(method), Some(id)) => Classified::ServerRequest {
                id,
                method,
                params: self.params.unwrap_or(Value::Null),
            },
            (Some(method), None) => Classified::Notification {
                method,
                params: self.params.unwrap_or(Value::Null),
            },
            (None, Some(id)) => Classified::Response {
                id,
                payload: match self.error {
                    Some(error) => Err(error),
                    None => Ok(self.result.unwrap_or(Value::Null)),
                },
            },
            (None, None) => Classified::Unknown(self.params.unwrap_or(Value::Null)),
        }
    }
}

/// Why the stream could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("could not encode an outgoing message: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("could not parse a server message: {0}")]
    Parse(serde_json::Error),
}

/// Parse one line into an [`Incoming`].
///
/// A line that is not valid JSON is reported rather than skipped: the stream is
/// newline-delimited JSON, so anything else means the protocol was violated and
/// continuing would hide it.
pub fn parse_line(line: &str) -> Result<Incoming, ProtocolError> {
    serde_json::from_str(line).map_err(ProtocolError::Parse)
}

/// The error code for "method not found", which several providers use to mean
/// "this build is too old for that call".
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Whether an error means the server does not support a method at all.
///
/// Both providers use the same idea: an older build answers a method it does
/// not know with a distinct code and wording rather than a generic failure, so
/// a caller can fall back instead of reporting a bug.
pub fn is_unavailable_method(error: &IncomingError) -> bool {
    if error.code == METHOD_NOT_FOUND {
        return true;
    }
    let message = error.message.to_ascii_lowercase();
    [
        "method not found",
        "unsupported method",
        "unknown method",
        "not implemented",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_uses_the_fixed_ids() {
        let init = Outgoing::initialize("FlowX", "0.1.0");
        match &init {
            Outgoing::Request { id, method, .. } => {
                assert_eq!(*id, INITIALIZE_ID);
                assert_eq!(method, "initialize");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Outgoing::initialized(),
            Outgoing::Notification { .. }
        ));
    }

    #[test]
    fn an_outgoing_message_encodes_to_one_newline_terminated_line() {
        let bytes = Outgoing::initialize("FlowX", "1").encode().unwrap();
        assert_eq!(*bytes.last().unwrap(), b'\n');
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 1);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"jsonrpc\":\"2.0\""), "{text}");
        assert!(text.contains("\"experimentalApi\":true"), "{text}");
    }

    #[test]
    fn every_message_kind_carries_the_jsonrpc_version_and_no_tag() {
        // JSON-RPC has no `type` field. A derived internally tagged enum would
        // emit one and be rejected by the server.
        let cases = vec![
            Outgoing::initialize("FlowX", "1"),
            Outgoing::initialized(),
            Outgoing::Request {
                id: 12,
                method: "thread/list".into(),
                params: Some(json!({ "limit": 5 })),
            },
            Outgoing::Response {
                id: json!(7),
                result: json!({}),
            },
            Outgoing::ErrorResponse {
                id: json!(7),
                code: -32601,
                message: "FlowX does not support that.".into(),
            },
        ];
        for message in cases {
            let json: Value =
                serde_json::from_str(&String::from_utf8(message.encode().unwrap()).unwrap())
                    .expect("the encoding must be valid JSON");
            assert_eq!(json["jsonrpc"], "2.0", "{json}");
            assert!(
                json.get("type").is_none(),
                "a derived tag leaked into the wire form: {json}"
            );
        }
    }

    #[test]
    fn a_request_carries_id_method_and_params() {
        let json = Outgoing::Request {
            id: 12,
            method: "thread/list".into(),
            params: Some(json!({ "limit": 5 })),
        };
        let encoded: Value = serde_json::from_slice(&Outgoing::serialize_check(&json)).unwrap();
        assert_eq!(encoded["id"], 12);
        assert_eq!(encoded["method"], "thread/list");
        assert_eq!(encoded["params"]["limit"], 5);
    }

    #[test]
    fn a_notification_carries_no_id() {
        let encoded = String::from_utf8(Outgoing::initialized().encode().unwrap()).unwrap();
        let json: Value = serde_json::from_str(&encoded).unwrap();
        assert!(json.get("id").is_none(), "{json}");
        assert_eq!(json["method"], "initialized");
    }

    #[test]
    fn a_response_carries_a_result_and_no_method() {
        let encoded = String::from_utf8(
            Outgoing::Response {
                id: json!(7),
                result: json!({ "decision": "accept" }),
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
        let json: Value = serde_json::from_str(&encoded).unwrap();
        assert!(json.get("method").is_none(), "{json}");
        assert_eq!(json["result"]["decision"], "accept");
    }

    #[test]
    fn an_error_response_nests_code_and_message() {
        let encoded = String::from_utf8(
            Outgoing::ErrorResponse {
                id: json!(7),
                code: -32601,
                message: "FlowX does not support x.".into(),
            }
            .encode()
            .unwrap(),
        )
        .unwrap();
        let json: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(json["error"]["code"], -32601);
        assert_eq!(json["error"]["message"], "FlowX does not support x.");
    }

    #[test]
    fn request_ids_start_above_the_fixed_ones() {
        // Fixed ids 0 and 1 must never collide with a dynamic request.
        let mut counter = FIRST_DYNAMIC_ID;
        let first = Outgoing::next_id(&mut counter);
        assert!(
            first > THREAD_ID,
            "{first} could collide with the handshake"
        );
        assert_eq!(Outgoing::next_id(&mut counter), first + 1);
    }

    #[test]
    fn a_response_is_classified_by_its_id() {
        let incoming: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).unwrap();
        match incoming.classify() {
            Classified::Response { payload, .. } => {
                assert!(payload.unwrap()["ok"].as_bool().unwrap());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_error_response_carries_its_code_and_message() {
        let incoming: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"boom"}}"#,
        )
        .unwrap();
        match incoming.classify() {
            Classified::Response {
                payload: Err(e), ..
            } => {
                assert_eq!(e.code, -32603);
                assert_eq!(e.message, "boom");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_notification_has_a_method_but_no_id() {
        let incoming: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
        )
        .unwrap();
        match incoming.classify() {
            Classified::Notification { method, params } => {
                assert_eq!(method, "turn/started");
                assert_eq!(params["turn"]["id"], "t1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_server_request_carries_both_and_must_be_answered() {
        let incoming: Incoming = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"method":"item/commandExecution/requestApproval","params":{"cmd":"ls"}}"#,
        )
        .unwrap();
        match incoming.classify() {
            Classified::ServerRequest { id, method, params } => {
                assert_eq!(id, json!(7));
                assert_eq!(method, "item/commandExecution/requestApproval");
                assert_eq!(params["cmd"], "ls");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_message_with_neither_id_nor_method_is_unknown_rather_than_fatal() {
        let incoming: Incoming = serde_json::from_str(r#"{"jsonrpc":"2.0"}"#).unwrap();
        assert!(matches!(incoming.classify(), Classified::Unknown(_)));
    }

    #[test]
    fn missing_params_default_to_null() {
        let incoming: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"thread/compacted"}"#).unwrap();
        match incoming.classify() {
            Classified::Notification { params, .. } => assert!(params.is_null()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unavailable_method_is_recognized_by_code_or_by_wording() {
        assert!(is_unavailable_method(&IncomingError {
            code: METHOD_NOT_FOUND,
            message: "whatever".into()
        }));
        for wording in [
            "method not found",
            "Unsupported method",
            "unknown method xyz",
            "not implemented",
        ] {
            assert!(
                is_unavailable_method(&IncomingError {
                    code: -32603,
                    message: wording.into()
                }),
                "{wording:?} was not recognized"
            );
        }
        assert!(!is_unavailable_method(&IncomingError {
            code: -32603,
            message: "thread existing-task already has an active writer".into()
        }));
    }

    #[test]
    fn a_malformed_line_is_reported_rather_than_skipped() {
        assert!(matches!(
            parse_line("{not json"),
            Err(ProtocolError::Parse(_))
        ));
    }

    #[test]
    fn a_well_formed_line_parses() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"thread":{"id":"t"}}}"#;
        assert!(parse_line(line).is_ok());
    }
}
