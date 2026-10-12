use serde::{Deserialize, Serialize};

/// How risky a tool call looks, which drives how the approval is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolRiskLevel {
    Safe,
    #[default]
    Moderate,
    Dangerous,
}

impl ToolRiskLevel {
    pub const ALL: [Self; 3] = [Self::Safe, Self::Moderate, Self::Dangerous];
}

/// Where an approval request is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalStatus {
    #[default]
    Pending,
    Approved,
    Denied,
}

impl ApprovalStatus {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Approved, Self::Denied];

    pub fn is_pending(self) -> bool {
        self == Self::Pending
    }
}

/// A tool call waiting on the user.
///
/// Structured questions are deliberately *not* modelled as approvals: they are
/// always shown and always require an answer, regardless of the thread's access
/// level. See [`crate::models::AgentAccess`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolApprovalRequest {
    pub id: uuid::Uuid,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: std::collections::BTreeMap<String, String>,
    #[serde(rename = "riskLevel", default)]
    pub risk_level: ToolRiskLevel,
    #[serde(rename = "agentID")]
    pub agent_id: uuid::Uuid,
    #[serde(default)]
    pub status: ApprovalStatus,
}

impl ToolApprovalRequest {
    pub fn new(
        tool_name: impl Into<String>,
        description: impl Into<String>,
        agent_id: uuid::Uuid,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            tool_name: tool_name.into(),
            description: description.into(),
            parameters: std::collections::BTreeMap::new(),
            risk_level: ToolRiskLevel::default(),
            agent_id,
            status: ApprovalStatus::default(),
        }
    }

    /// The parameters rendered as `key: value` lines, in key order so the
    /// display is stable across frames.
    pub fn parameter_lines(&self) -> Vec<String> {
        self.parameters
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect()
    }
}

/// A structured question from the provider, and the answer given to it.
///
/// Providers ask up to a bounded number of questions with a bounded number of
/// options each, so the caps live here rather than in the UI that renders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputRequest {
    pub id: uuid::Uuid,
    #[serde(rename = "agentID")]
    pub agent_id: uuid::Uuid,
    pub questions: Vec<UserInputQuestion>,
}

impl UserInputRequest {
    /// The most questions any supported provider asks in one turn.
    pub const MAX_QUESTIONS: usize = 3;
    /// The most options any one question offers.
    pub const MAX_OPTIONS_PER_QUESTION: usize = 10;

    pub fn new(agent_id: uuid::Uuid, questions: Vec<UserInputQuestion>) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            agent_id,
            questions,
        }
    }
}

/// One question, with its allowed answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputQuestion {
    /// The provider's own identifier, echoed back in the reply.
    pub id: String,
    /// A short label for the group of options.
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    /// Whether the provider expects free text beyond the listed options.
    #[serde(default, rename = "isOther")]
    pub is_other: bool,
    /// Whether the answer must not be echoed back or logged.
    #[serde(default, rename = "isSecret")]
    pub is_secret: bool,
}

/// A selectable answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// The answer to one question, plus how long to keep waiting before proceeding
/// on the user's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputResponse {
    pub question_id: String,
    /// The chosen labels, in selection order.
    pub answers: Vec<String>,
}

impl UserInputResponse {
    pub fn new(question_id: impl Into<String>, answers: Vec<String>) -> Self {
        Self {
            question_id: question_id.into(),
            answers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_request_starts_pending_and_moderate() {
        let r = ToolApprovalRequest::new("shell", "Run a command", uuid::Uuid::nil());
        assert!(r.status.is_pending());
        assert_eq!(r.risk_level, ToolRiskLevel::Moderate);
        assert!(r.parameters.is_empty());
    }

    #[test]
    fn parameter_lines_are_ordered_and_formatted() {
        let mut r = ToolApprovalRequest::new("shell", "", uuid::Uuid::nil());
        r.parameters.insert("command".into(), "ls -la".into());
        r.parameters.insert("cwd".into(), "/tmp".into());
        assert_eq!(r.parameter_lines(), vec!["command: ls -la", "cwd: /tmp"]);
    }

    #[test]
    fn a_request_round_trips_with_its_parameters() {
        let mut r = ToolApprovalRequest::new("edit", "Change a file", uuid::Uuid::nil());
        r.parameters.insert("path".into(), "src/lib.rs".into());
        r.risk_level = ToolRiskLevel::Dangerous;
        r.status = ApprovalStatus::Denied;

        let json = serde_json::to_string(&r).unwrap();
        let back: ToolApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn a_payload_missing_optional_fields_still_decodes() {
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","toolName":"shell","description":"","agentID":"00000000-0000-0000-0000-000000000000"}"#;
        let r: ToolApprovalRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.risk_level, ToolRiskLevel::Moderate);
        assert_eq!(r.status, ApprovalStatus::Pending);
    }

    #[test]
    fn input_requests_preserve_question_metadata() {
        let req = UserInputResponse::new("q1", vec!["Yes".into()]);
        let json = serde_json::to_string(&req).unwrap();
        let back: UserInputResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }
}
