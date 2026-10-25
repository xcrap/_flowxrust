//! The agent's own task list for a turn.
//!
//! Both providers keep one, in different shapes:
//!
//! - Claude calls its `TodoWrite` tool with
//!   `{"todos":[{"content","status","activeForm"}]}`, the whole list each time.
//! - Codex calls `update_plan` and reports it as a `turn/plan/updated`
//!   notification carrying the same tool's arguments:
//!   `{"explanation","plan":[{"step","status"}]}`.
//!
//! Reading both into one model here means the view draws one checklist and
//! never learns which provider wrote it. Statuses are read leniently: Codex
//! writes `inProgress`, its older builds and Claude write `in_progress`, and an
//! unknown status is a step not started rather than a plan that cannot be read.

use serde_json::{Value, json};

/// The tool Claude calls to rewrite its task list.
pub const TODO_TOOL_NAME: &str = "TodoWrite";
/// The tool Codex calls to rewrite its plan.
pub const PLAN_TOOL_NAME: &str = "update_plan";

/// Where one step of a plan is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    /// Read a provider's status, whichever spelling it uses.
    pub fn parse(value: &str) -> Self {
        let normalized: String = value
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match normalized.as_str() {
            "inprogress" | "active" | "running" => Self::InProgress,
            "completed" | "complete" | "done" => Self::Completed,
            _ => Self::Pending,
        }
    }

    /// The spelling written back into a recorded plan.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    /// What the step says while it is the one being worked on: Claude's
    /// `activeForm`, "Running the tests" for "Run the tests".
    pub active_text: Option<String>,
    pub status: PlanStatus,
}

impl PlanStep {
    /// The words to show for this step in its current state.
    pub fn label(&self) -> &str {
        match (&self.active_text, self.status) {
            (Some(active), PlanStatus::InProgress) => active,
            _ => &self.text,
        }
    }
}

/// A turn's task list, as the agent last wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnPlan {
    /// Codex's note on why the plan changed, when it gave one.
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

impl TurnPlan {
    /// Whether a tool call is one that rewrites the plan.
    pub fn is_plan_tool(name: &str) -> bool {
        name.eq_ignore_ascii_case(TODO_TOOL_NAME) || name.eq_ignore_ascii_case(PLAN_TOOL_NAME)
    }

    /// The plan a tool call wrote, or `None` when it is not a plan call or its
    /// arguments do not read as one.
    pub fn from_tool_call(name: &str, input: &str) -> Option<Self> {
        if !Self::is_plan_tool(name) {
            return None;
        }
        Self::from_value(&serde_json::from_str(input).ok()?)
    }

    /// Read either provider's shape: `todos` or `plan`.
    pub fn from_value(value: &Value) -> Option<Self> {
        let items = value
            .get("todos")
            .or_else(|| value.get("plan"))
            .and_then(Value::as_array)?;
        let text = |item: &Value, key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let steps = items
            .iter()
            .filter_map(|item| {
                Some(PlanStep {
                    // A step with nothing to say is not a step.
                    text: text(item, "content").or_else(|| text(item, "step"))?,
                    active_text: text(item, "activeForm"),
                    status: PlanStatus::parse(
                        item.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                })
            })
            .collect();
        Some(Self {
            explanation: text(value, "explanation"),
            steps,
        })
    }

    /// The arguments `update_plan` would have been called with, so a plan that
    /// arrived as a notification can be recorded as the call it stands for.
    pub fn to_tool_input(&self) -> String {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| {
                let mut entry = json!({ "step": step.text, "status": step.status.wire() });
                if let Some(active) = &step.active_text {
                    entry["activeForm"] = json!(active);
                }
                entry
            })
            .collect();
        let mut input = json!({ "plan": steps });
        if let Some(explanation) = &self.explanation {
            input["explanation"] = json!(explanation);
        }
        input.to_string()
    }

    /// How many steps are done.
    pub fn completed(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.status == PlanStatus::Completed)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_todo_write_call_reads_as_a_plan() {
        let input = r#"{"todos":[
            {"content":"Read the parser","status":"completed","activeForm":"Reading the parser"},
            {"content":"Fix the bug","status":"in_progress","activeForm":"Fixing the bug"},
            {"content":"Run the tests","status":"pending","activeForm":"Running the tests"}
        ]}"#;
        let plan = TurnPlan::from_tool_call("TodoWrite", input).expect("a plan");
        let labels: Vec<(&str, PlanStatus)> = plan
            .steps
            .iter()
            .map(|step| (step.label(), step.status))
            .collect();
        assert_eq!(
            labels,
            [
                ("Read the parser", PlanStatus::Completed),
                ("Fixing the bug", PlanStatus::InProgress),
                ("Run the tests", PlanStatus::Pending),
            ],
            "only the step in progress speaks in its active form"
        );
        assert_eq!(plan.completed(), 1);
    }

    #[test]
    fn a_codex_plan_reads_as_the_same_model_and_round_trips() {
        let params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "explanation": "Found the cause",
            "plan": [
                {"step": "Inspect", "status": "completed"},
                {"step": "Patch", "status": "inProgress"},
                {"step": "Verify", "status": "pending"},
            ],
        });
        let plan = TurnPlan::from_value(&params).expect("a plan");
        assert_eq!(plan.explanation.as_deref(), Some("Found the cause"));
        assert_eq!(
            plan.steps.iter().map(|s| s.status).collect::<Vec<_>>(),
            [
                PlanStatus::Completed,
                PlanStatus::InProgress,
                PlanStatus::Pending
            ]
        );
        let recorded = TurnPlan::from_tool_call(PLAN_TOOL_NAME, &plan.to_tool_input());
        assert_eq!(recorded, Some(plan), "a recorded plan reads back unchanged");
    }

    #[test]
    fn statuses_are_read_in_every_spelling_and_unknown_ones_are_pending() {
        for (value, expected) in [
            ("in_progress", PlanStatus::InProgress),
            ("inProgress", PlanStatus::InProgress),
            ("IN-PROGRESS", PlanStatus::InProgress),
            ("completed", PlanStatus::Completed),
            ("pending", PlanStatus::Pending),
            ("", PlanStatus::Pending),
            ("blocked", PlanStatus::Pending),
        ] {
            assert_eq!(PlanStatus::parse(value), expected, "{value:?}");
        }
    }

    #[test]
    fn other_tools_and_unreadable_arguments_are_not_plans() {
        assert_eq!(TurnPlan::from_tool_call("Read", r#"{"todos":[]}"#), None);
        assert_eq!(TurnPlan::from_tool_call("TodoWrite", "not json"), None);
        assert_eq!(
            TurnPlan::from_tool_call("TodoWrite", r#"{"items":[]}"#),
            None
        );
        // A step with no words is dropped rather than drawn as a blank row.
        let plan = TurnPlan::from_tool_call(
            "todowrite",
            r#"{"todos":[{"content":"  ","status":"pending"}]}"#,
        )
        .expect("still a plan");
        assert!(plan.is_empty());
    }
}
