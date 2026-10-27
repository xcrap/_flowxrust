//! MCP elicitation: an MCP server asking the user for something, relayed by
//! the provider that runs it.
//!
//! Both providers relay it, in different envelopes. Claude Code sends a
//! `control_request` of subtype `elicitation` (`mcp_server_name`, `message`,
//! `mode`, `requested_schema`, `url`); `codex app-server` sends the request
//! `mcpServer/elicitation/request` (`serverName`, `message`, `mode`,
//! `requestedSchema`, `url`, `_meta`). Both take the MCP result back:
//! `{action: accept|decline|cancel, content}`. This module is the one reader
//! for both, so a form cannot be drawn one way for Claude and answered
//! another way for Codex.
//!
//! Three rules, each a bug first:
//!
//! - **Refusing is not neutral.** FlowX once answered every elicitation with
//!   an error, which Claude turns into `cancel` and Codex into a failed call.
//!   The server's tool then failed with nothing on screen to say it had asked.
//! - **Codex asks for MCP tool approvals this way.** Under any access level
//!   that asks, Codex 0.164 approves an MCP tool call through an elicitation
//!   whose `_meta.codex_approval_kind` is `mcp_tool_call` and whose form is
//!   empty. It is a tool approval, so it is drawn and answered as one, and an
//!   approval policy may answer it. Refused, every MCP tool call failed.
//! - **Every other elicitation is the reader's to answer.** It asks for data
//!   or consent only the person at the keyboard has, so no approval policy
//!   answers it, Full Access included.

use serde_json::{Map, Value, json};

use crate::provider::{
    ApprovalRequest, Option_, ProviderEvent, Question, RiskLevel, UserInputRequest, UserResponse,
};

/// The most fields taken from one form.
pub const MAX_FIELDS: usize = 12;
/// The most choices one field offers.
pub const MAX_CHOICES: usize = 24;

/// What the reader is told to write for a yes-or-no field.
const YES: &str = "Yes";
const NO: &str = "No";

/// One elicitation, read from either provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Elicitation {
    /// The MCP server that asked.
    pub server: String,
    /// What it said.
    pub message: String,
    pub form: Form,
}

/// What an elicitation asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Form {
    /// Nothing to fill in: the reader accepts or declines what the message says.
    Confirm,
    /// Fields to fill in.
    Fields(Vec<Field>),
    /// A page the reader is asked to open (MCP's `url` mode). Accepting is
    /// consenting to open it.
    Link { url: String },
    /// Codex asking whether an MCP tool may run.
    ToolApproval {
        /// The tool's name, when Codex said it.
        tool: Option<String>,
        /// The arguments it would run with.
        arguments: Value,
    },
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// The property name, which keys the answer.
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: FieldKind,
    pub required: bool,
}

/// The JSON type a field's answer is sent as.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    Integer,
    Number,
    Boolean,
    /// One of these, as `(value, label)`.
    Choice(Vec<(String, String)>),
    /// Any of these, as `(value, label)`.
    Choices(Vec<(String, String)>),
}

/// The outcome to send back, before it is wrapped for a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// `accept`, `decline` or `cancel`.
    pub action: &'static str,
    pub content: Option<Value>,
}

impl Reply {
    /// The reader said no.
    pub fn declined() -> Self {
        Self {
            action: "decline",
            content: None,
        }
    }

    /// Nobody answered: the turn ended, or there was no one to ask.
    pub fn cancelled() -> Self {
        Self {
            action: "cancel",
            content: None,
        }
    }

    /// The shape Claude Code reads. `content` is omitted rather than null,
    /// because its schema takes an absent record and refuses a null one.
    pub fn to_claude(&self) -> Value {
        let mut reply = json!({ "action": self.action });
        if let Some(content) = &self.content {
            reply["content"] = content.clone();
        }
        reply
    }

    /// The shape `codex app-server` reads, which names every field.
    pub fn to_codex(&self) -> Value {
        json!({
            "action": self.action,
            "content": self.content.clone().unwrap_or(Value::Null),
            "_meta": Value::Null,
        })
    }
}

impl Elicitation {
    /// Read Claude's `control_request.request`.
    pub fn from_claude(request: &Value) -> Self {
        Self::read(
            text(request.get("mcp_server_name")),
            text(request.get("message")),
            request.get("mode").and_then(Value::as_str),
            request.get("requested_schema"),
            request.get("url").and_then(Value::as_str),
            None,
        )
    }

    /// Read Codex's `mcpServer/elicitation/request` params.
    pub fn from_codex(params: &Value) -> Self {
        Self::read(
            text(params.get("serverName")),
            text(params.get("message")),
            params.get("mode").and_then(Value::as_str),
            params.get("requestedSchema"),
            params.get("url").and_then(Value::as_str),
            params.get("_meta"),
        )
    }

    fn read(
        server: String,
        message: String,
        mode: Option<&str>,
        schema: Option<&Value>,
        url: Option<&str>,
        meta: Option<&Value>,
    ) -> Self {
        let server = bounded(
            if server.is_empty() {
                "An MCP server".into()
            } else {
                server
            },
            256,
        );
        let message = bounded(message, 8_192);
        let approval = meta
            .and_then(|meta| meta.get("codex_approval_kind"))
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.is_empty());
        let form = match (mode, url) {
            (Some("url"), Some(url)) if is_web_link(url) => Form::Link {
                url: bounded(url.to_owned(), 4_096),
            },
            _ if approval => Form::ToolApproval {
                tool: quoted_name(&message),
                arguments: meta
                    .and_then(|meta| meta.get("tool_params"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            },
            _ => {
                let fields = schema.map(fields).unwrap_or_default();
                if fields.is_empty() {
                    Form::Confirm
                } else {
                    Form::Fields(fields)
                }
            }
        };
        Self {
            server,
            message,
            form,
        }
    }

    /// Whether a configured approval policy may answer this, as it answers
    /// a tool call. Only Codex's tool approvals: everything else asks for
    /// something only the reader has.
    pub fn is_tool_approval(&self) -> bool {
        matches!(self.form, Form::ToolApproval { .. })
    }

    /// What the conversation and the views are shown, under the id the
    /// caller will be answered by.
    pub fn event(&self, id: String) -> ProviderEvent {
        match &self.form {
            Form::ToolApproval { tool, arguments } => ProviderEvent::Approval(ApprovalRequest {
                id,
                tool_name: match tool {
                    Some(tool) => format!("mcp__{}__{tool}", self.server),
                    None => format!("mcp__{}", self.server),
                },
                input: arguments.clone(),
                risk: RiskLevel::Moderate,
            }),
            form => ProviderEvent::UserInput(UserInputRequest {
                id,
                questions: match form {
                    Form::Fields(fields) => fields.iter().map(Field::question).collect(),
                    _ => Vec::new(),
                },
                source: Some(self.server.clone()),
                message: Some(self.message.clone()).filter(|message| !message.is_empty()),
                link: match form {
                    Form::Link { url } => Some(url.clone()),
                    _ => None,
                },
            }),
        }
    }

    /// The reply to the reader's answer, or `None` for an answer of the wrong
    /// kind, which leaves the request open.
    pub fn reply(&self, response: &UserResponse) -> Option<Reply> {
        match (&self.form, response) {
            (Form::ToolApproval { .. }, UserResponse::Approval { approved, .. }) => {
                Some(if *approved {
                    Reply {
                        action: "accept",
                        content: None,
                    }
                } else {
                    Reply::declined()
                })
            }
            (Form::ToolApproval { .. }, _) | (_, UserResponse::Approval { .. }) => None,
            (_, UserResponse::CancelQuestions { .. }) => Some(Reply::declined()),
            (Form::Fields(fields), UserResponse::Questions { answers, .. }) => Some(Reply {
                action: "accept",
                content: Some(content(fields, answers)),
            }),
            (Form::Confirm | Form::Link { .. }, UserResponse::Questions { .. }) => Some(Reply {
                action: "accept",
                content: None,
            }),
        }
    }
}

impl Field {
    /// The question the dialog asks for this field.
    fn question(&self) -> Question {
        let hint = match &self.kind {
            FieldKind::Integer => "Enter a whole number.",
            FieldKind::Number => "Enter a number.",
            FieldKind::Choices(_) => "Choose one or more, separated by commas.",
            _ => "",
        };
        let question = match (self.description.is_empty(), hint.is_empty()) {
            (false, false) => format!("{} {hint}", self.description),
            (false, true) => self.description.clone(),
            (true, _) => hint.to_owned(),
        };
        let options = match &self.kind {
            FieldKind::Boolean => vec![option(YES), option(NO)],
            FieldKind::Choice(choices) | FieldKind::Choices(choices) => {
                choices.iter().map(|(_, label)| option(label)).collect()
            }
            _ => Vec::new(),
        };
        Question {
            id: self.name.clone(),
            header: self.title.clone(),
            question,
            options,
            // A single choice and a yes-or-no are answered by choosing; a
            // multiple choice is typed, because the dialog picks one row.
            is_other: !matches!(self.kind, FieldKind::Boolean | FieldKind::Choice(_)),
            is_secret: false,
            optional: !self.required,
        }
    }

    /// The typed value for one answer. Text that does not read as the type
    /// asked for is sent as written: the server validates its own schema, and
    /// a reply that never leaves would leave it waiting.
    fn value(&self, answer: &str) -> Value {
        match &self.kind {
            FieldKind::Text => Value::String(answer.to_owned()),
            FieldKind::Integer => answer
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::String(answer.to_owned())),
            FieldKind::Number => answer
                .trim()
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or_else(|| Value::String(answer.to_owned())),
            FieldKind::Boolean => match answer.trim().to_ascii_lowercase().as_str() {
                "yes" | "true" => Value::Bool(true),
                "no" | "false" => Value::Bool(false),
                _ => Value::String(answer.to_owned()),
            },
            FieldKind::Choice(choices) => Value::String(chosen(choices, answer)),
            FieldKind::Choices(choices) => Value::Array(
                answer
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| Value::String(chosen(choices, part)))
                    .collect(),
            ),
        }
    }
}

/// The form's answers as the object its schema describes. A blank optional
/// field is left out rather than sent empty.
fn content(fields: &[Field], answers: &[(String, String)]) -> Value {
    let mut content = Map::new();
    for field in fields {
        let Some((_, answer)) = answers.iter().find(|(id, _)| *id == field.name) else {
            continue;
        };
        if answer.trim().is_empty() && !field.required {
            continue;
        }
        content.insert(field.name.clone(), field.value(answer));
    }
    Value::Object(content)
}

/// The value for a label the reader chose, or for a value typed directly.
fn chosen(choices: &[(String, String)], answer: &str) -> String {
    let answer = answer.trim();
    choices
        .iter()
        .find(|(_, label)| label == answer)
        .or_else(|| choices.iter().find(|(value, _)| value == answer))
        .map(|(value, _)| value.clone())
        .unwrap_or_else(|| answer.to_owned())
}

/// A form's fields, in the order the schema lists them.
///
/// Properties of a type the dialog cannot ask for are left out; a required
/// one still is, because MCP restricts forms to flat primitives and a server
/// that breaks that is told so by its own validation.
fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    properties
        .iter()
        .filter_map(|(name, property)| {
            let kind = kind(property)?;
            Some(Field {
                name: name.clone(),
                title: bounded(
                    property
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.trim().is_empty())
                        .unwrap_or(name)
                        .to_owned(),
                    256,
                ),
                description: bounded(text(property.get("description")), 2_048),
                kind,
                required: required.contains(&name.as_str()),
            })
        })
        .take(MAX_FIELDS)
        .collect()
}

fn kind(property: &Value) -> Option<FieldKind> {
    if let Some(choices) = choices(property) {
        return Some(FieldKind::Choice(choices));
    }
    match property.get("type").and_then(Value::as_str)? {
        "string" => Some(FieldKind::Text),
        "integer" => Some(FieldKind::Integer),
        "number" => Some(FieldKind::Number),
        "boolean" => Some(FieldKind::Boolean),
        "array" => property
            .get("items")
            .and_then(choices)
            .map(FieldKind::Choices),
        _ => None,
    }
}

/// A property's choices: `oneOf`/`anyOf` of `{const, title}`, or `enum` with
/// the legacy `enumNames` labels.
fn choices(property: &Value) -> Option<Vec<(String, String)>> {
    if let Some(options) = property
        .get("oneOf")
        .or_else(|| property.get("anyOf"))
        .and_then(Value::as_array)
    {
        let choices: Vec<_> = options
            .iter()
            .filter_map(|option| {
                let value = option.get("const").and_then(Value::as_str)?;
                let label = option.get("title").and_then(Value::as_str).unwrap_or(value);
                Some((value.to_owned(), bounded(label.to_owned(), 256)))
            })
            .take(MAX_CHOICES)
            .collect();
        return (!choices.is_empty()).then_some(choices);
    }
    let values = property.get("enum").and_then(Value::as_array)?;
    let names = property.get("enumNames").and_then(Value::as_array);
    let choices: Vec<_> = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            let value = value.as_str()?;
            let label = names
                .and_then(|names| names.get(index))
                .and_then(Value::as_str)
                .unwrap_or(value);
            Some((value.to_owned(), bounded(label.to_owned(), 256)))
        })
        .take(MAX_CHOICES)
        .collect();
    (!choices.is_empty()).then_some(choices)
}

/// The last double-quoted name in Codex's approval message, which is the only
/// place it names the tool: `Allow the probe MCP server to run tool "ask_name"?`
fn quoted_name(message: &str) -> Option<String> {
    let end = message.rfind('"')?;
    let start = message[..end].rfind('"')?;
    let name = &message[start + 1..end];
    (!name.is_empty() && name.len() <= 256 && !name.contains(char::is_whitespace))
        .then(|| name.to_owned())
}

/// Only a web page is offered for opening. Anything else — a `file:` URL, an
/// app's own scheme — would be FlowX launching what a server named.
pub fn is_web_link(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && !url.chars().any(char::is_control)
}

fn option(label: &str) -> Option_ {
    Option_ {
        label: label.to_owned(),
        description: String::new(),
    }
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

fn bounded(mut value: String, limit: usize) -> String {
    if value.len() > limit {
        let mut end = limit;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The form the probe MCP server sent, as Claude Code 2.1.283 relayed it.
    fn claude_form() -> Value {
        json!({
            "subtype": "elicitation", "mcp_server_name": "probe", "message": "Who is asking?",
            "mode": "form", "requested_schema": {"type": "object", "properties": {
                "name": {"type": "string", "title": "Name", "description": "Your name"},
                "color": {"type": "string", "title": "Colour", "enum": ["red", "blue"]},
                "agree": {"type": "boolean", "title": "Agree"},
                "age": {"type": "integer", "title": "Age"}},
                "required": ["name"]}
        })
    }

    /// Codex 0.164's approval for an MCP tool call, captured from a live run.
    fn codex_tool_approval() -> Value {
        json!({
            "threadId": "t", "turnId": "u", "serverName": "probe", "mode": "form",
            "_meta": {"codex_approval_kind": "mcp_tool_call", "persist": ["session", "always"],
                      "tool_description": "Asks the user for their name and favourite colour.",
                      "tool_params": {}, "tool_params_display": []},
            "message": "Allow the probe MCP server to run tool \"ask_name\"?",
            "requestedSchema": {"type": "object", "properties": {}}
        })
    }

    fn answers(pairs: &[(&str, &str)]) -> UserResponse {
        UserResponse::Questions {
            id: "e".into(),
            answers: pairs
                .iter()
                .map(|(id, answer)| (id.to_string(), answer.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_live_claude_form_becomes_one_question_per_field() {
        let elicitation = Elicitation::from_claude(&claude_form());
        let ProviderEvent::UserInput(request) = elicitation.event("e".into()) else {
            panic!("a form is a question");
        };
        assert_eq!(request.source.as_deref(), Some("probe"));
        assert_eq!(request.message.as_deref(), Some("Who is asking?"));
        let ids: Vec<_> = request.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        for id in ["name", "color", "agree", "age"] {
            assert!(ids.contains(&id), "{id} missing from {ids:?}");
        }
        let name = request.questions.iter().find(|q| q.id == "name").unwrap();
        assert_eq!(name.header, "Name");
        assert_eq!(name.question, "Your name");
        assert!(!name.optional && name.is_other);
        let color = request.questions.iter().find(|q| q.id == "color").unwrap();
        assert_eq!(
            color
                .options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["red", "blue"]
        );
        assert!(color.optional && !color.is_other);
        let agree = request.questions.iter().find(|q| q.id == "agree").unwrap();
        assert_eq!(agree.options.len(), 2);
    }

    #[test]
    fn answers_are_sent_as_the_types_the_schema_names() {
        let elicitation = Elicitation::from_claude(&claude_form());
        let reply = elicitation
            .reply(&answers(&[
                ("name", "Ada"),
                ("color", "blue"),
                ("agree", "Yes"),
                ("age", " 36 "),
            ]))
            .unwrap();
        assert_eq!(
            reply.to_claude(),
            json!({"action": "accept", "content": {"name": "Ada", "color": "blue", "agree": true, "age": 36}})
        );
    }

    #[test]
    fn a_blank_optional_field_is_left_out_and_a_bad_number_is_sent_as_written() {
        let elicitation = Elicitation::from_claude(&claude_form());
        let reply = elicitation
            .reply(&answers(&[
                ("name", "Ada"),
                ("color", ""),
                ("agree", ""),
                ("age", "thirty"),
            ]))
            .unwrap();
        assert_eq!(reply.content, Some(json!({"name": "Ada", "age": "thirty"})));
    }

    #[test]
    fn titled_choices_send_their_value_and_show_their_label() {
        let elicitation = Elicitation::from_codex(&json!({
            "serverName": "deploy", "message": "Where?", "mode": "form",
            "requestedSchema": {"type": "object", "properties": {
                "region": {"type": "string", "oneOf": [
                    {"const": "eu-west-1", "title": "Ireland"},
                    {"const": "us-east-1", "title": "Virginia"}]},
                "tiers": {"type": "array", "items": {"anyOf": [
                    {"const": "a", "title": "Alpha"}, {"const": "b", "title": "Beta"}]}},
                "legacy": {"type": "string", "enum": ["x", "y"], "enumNames": ["Ex", "Why"]}},
                "required": ["region"]}
        }));
        let ProviderEvent::UserInput(request) = elicitation.event("e".into()) else {
            panic!("a form is a question");
        };
        let region = request.questions.iter().find(|q| q.id == "region").unwrap();
        assert_eq!(region.options[0].label, "Ireland");
        let reply = elicitation
            .reply(&answers(&[
                ("region", "Ireland"),
                ("tiers", "Alpha, b"),
                ("legacy", "Why"),
            ]))
            .unwrap();
        assert_eq!(
            reply.to_codex(),
            json!({"action": "accept", "_meta": null, "content": {
                "region": "eu-west-1", "tiers": ["a", "b"], "legacy": "y"}})
        );
    }

    #[test]
    fn cancelling_the_dialog_declines_and_no_answer_is_a_cancel() {
        let elicitation = Elicitation::from_claude(&claude_form());
        assert_eq!(
            elicitation
                .reply(&UserResponse::CancelQuestions { id: "e".into() })
                .unwrap()
                .to_claude(),
            json!({"action": "decline"})
        );
        assert_eq!(
            Reply::cancelled().to_codex(),
            json!({"action": "cancel", "content": null, "_meta": null})
        );
    }

    #[test]
    fn a_form_with_no_fields_is_a_confirmation() {
        let elicitation = Elicitation::from_claude(&json!({
            "mcp_server_name": "db", "message": "Drop the staging tables?", "mode": "form",
            "requested_schema": {"type": "object", "properties": {}}
        }));
        assert_eq!(elicitation.form, Form::Confirm);
        let ProviderEvent::UserInput(request) = elicitation.event("e".into()) else {
            panic!("a confirmation is the reader's to answer");
        };
        assert!(request.questions.is_empty());
        assert_eq!(request.message.as_deref(), Some("Drop the staging tables?"));
        assert_eq!(
            elicitation.reply(&answers(&[])).unwrap().to_claude(),
            json!({"action": "accept"})
        );
    }

    #[test]
    fn a_url_request_offers_its_link_and_only_a_web_link() {
        let elicitation = Elicitation::from_claude(&json!({
            "mcp_server_name": "github", "message": "Sign in to continue.", "mode": "url",
            "url": "https://github.com/login/device", "elicitation_id": "x"
        }));
        let ProviderEvent::UserInput(request) = elicitation.event("e".into()) else {
            panic!("a link is the reader's to open");
        };
        assert_eq!(
            request.link.as_deref(),
            Some("https://github.com/login/device")
        );
        for unsafe_link in ["file:///etc/passwd", "x-app://open", "javascript:alert(1)"] {
            let elicitation = Elicitation::from_claude(&json!({
                "mcp_server_name": "s", "message": "Open this", "mode": "url", "url": unsafe_link
            }));
            assert_eq!(elicitation.form, Form::Confirm, "{unsafe_link}");
        }
    }

    #[test]
    fn codexs_mcp_tool_approval_is_an_approval_card() {
        let elicitation = Elicitation::from_codex(&codex_tool_approval());
        assert!(elicitation.is_tool_approval());
        let ProviderEvent::Approval(approval) = elicitation.event("codex:0".into()) else {
            panic!("a tool approval is an approval");
        };
        assert_eq!(approval.tool_name, "mcp__probe__ask_name");
        assert_eq!(approval.input, json!({}));
        assert_eq!(
            elicitation
                .reply(&UserResponse::Approval {
                    id: "codex:0".into(),
                    approved: true
                })
                .unwrap()
                .to_codex(),
            json!({"action": "accept", "content": null, "_meta": null})
        );
        assert_eq!(
            elicitation
                .reply(&UserResponse::Approval {
                    id: "codex:0".into(),
                    approved: false
                })
                .unwrap()
                .action,
            "decline"
        );
        // A question's answer is not an approval's, and leaves it open.
        assert!(elicitation.reply(&answers(&[])).is_none());
    }

    #[test]
    fn a_form_is_never_answered_as_an_approval() {
        let elicitation = Elicitation::from_claude(&claude_form());
        assert!(!elicitation.is_tool_approval());
        assert!(
            elicitation
                .reply(&UserResponse::Approval {
                    id: "e".into(),
                    approved: true
                })
                .is_none()
        );
    }

    #[test]
    fn an_unnamed_server_and_unknown_property_types_still_read() {
        let elicitation = Elicitation::from_codex(&json!({
            "message": "Pick", "mode": "form", "requestedSchema": {"type": "object",
                "properties": {"blob": {"type": "object"}, "n": {"type": "number"}}}
        }));
        assert_eq!(elicitation.server, "An MCP server");
        let Form::Fields(fields) = &elicitation.form else {
            panic!("the number field remains");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].kind, FieldKind::Number);
        let reply = elicitation.reply(&answers(&[("n", "2.5")])).unwrap();
        assert_eq!(reply.content, Some(json!({"n": 2.5})));
    }
}
