//! A thread's goal: the objective a provider keeps working towards across
//! turns, and how far it has got.
//!
//! Codex owns goals (`thread/goal/*`); FlowX mirrors the last one it was told
//! about so the row above the composer survives a restart without asking the
//! provider first. The wire spelling of every field is Codex's, because the
//! record is read from Codex and written back to disk unchanged.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where a goal stands.
///
/// Read through [`GoalStatus::parse`], so a status a newer Codex or a newer
/// FlowX wrote reads as active instead of failing the transcript it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    /// Codex's spelling, which is also what `thread/goal/set` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usageLimited",
            Self::BudgetLimited => "budgetLimited",
            Self::Complete => "complete",
        }
    }

    /// Read Codex's spelling. An unknown status reads as active, as the
    /// original did: a newer server's status is still a goal being pursued.
    pub fn parse(raw: &str) -> Self {
        match raw {
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "usageLimited" => Self::UsageLimited,
            "budgetLimited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => Self::Active,
        }
    }

    /// What the badge says.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Paused => "Paused",
            Self::Blocked => "Blocked",
            Self::UsageLimited => "Usage limited",
            Self::BudgetLimited => "Budget limited",
            Self::Complete => "Complete",
        }
    }
}

impl<'de> Deserialize<'de> for GoalStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// One thread's goal, as the provider last reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationGoal {
    pub thread_id: String,
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub tokens_used: u64,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub time_used_seconds: u64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

impl ConversationGoal {
    /// Read a goal from a Codex payload.
    ///
    /// `None` unless the three fields that make it a goal are present: a
    /// record without an objective is not something the row can show.
    pub fn from_codex(payload: &Value) -> Option<Self> {
        let thread_id = payload.get("threadId")?.as_str()?.to_string();
        let objective = payload.get("objective")?.as_str()?.to_string();
        let status = GoalStatus::parse(payload.get("status")?.as_str()?);
        let number = |key: &str| payload.get(key).and_then(Value::as_u64);
        Some(Self {
            thread_id,
            objective,
            status,
            tokens_used: number("tokensUsed").unwrap_or_default(),
            token_budget: number("tokenBudget"),
            time_used_seconds: number("timeUsedSeconds").unwrap_or_default(),
            created_at: payload
                .get("createdAt")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            updated_at: payload
                .get("updatedAt")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
        })
    }

    /// The token meter beside the objective, when there is one worth showing.
    pub fn usage_text(&self) -> Option<String> {
        match self.token_budget.filter(|budget| *budget > 0) {
            Some(budget) => Some(format!(
                "{}/{}",
                compact_count(self.tokens_used),
                compact_count(budget)
            )),
            None if self.tokens_used > 0 => {
                Some(format!("{} tokens", compact_count(self.tokens_used)))
            }
            None => None,
        }
    }
}

/// `12.3k`, `4.5M`: short enough for a monospaced meter.
fn compact_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unknown_saved_status_reads_as_active() {
        let goal: ConversationGoal = serde_json::from_value(json!({
            "threadId": "t",
            "objective": "ship it",
            "status": "awaitingReview",
        }))
        .expect("a newer status still reads");
        assert_eq!(goal.status, GoalStatus::Active);
        for status in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::UsageLimited,
            GoalStatus::BudgetLimited,
            GoalStatus::Complete,
        ] {
            let written = serde_json::to_value(status).unwrap();
            assert_eq!(written, status.as_str());
            assert_eq!(
                serde_json::from_value::<GoalStatus>(written).unwrap(),
                status
            );
        }
    }

    #[test]
    fn a_codex_goal_reads_with_codex_spelling() {
        let goal = ConversationGoal::from_codex(&json!({
            "threadId": "thread-1",
            "objective": "Ship the release",
            "status": "budgetLimited",
            "tokensUsed": 12_345,
            "tokenBudget": 50_000,
            "timeUsedSeconds": 90,
            "createdAt": 1_760_000_000,
            "updatedAt": 1_760_000_100,
        }))
        .unwrap();
        assert_eq!(goal.status, GoalStatus::BudgetLimited);
        assert_eq!(goal.usage_text().as_deref(), Some("12.3k/50.0k"));
        assert_eq!(goal.status.as_str(), "budgetLimited");
    }

    #[test]
    fn a_payload_without_an_objective_is_not_a_goal() {
        assert!(
            ConversationGoal::from_codex(&json!({ "threadId": "t", "status": "active" })).is_none()
        );
        assert_eq!(GoalStatus::parse("somethingNew"), GoalStatus::Active);
    }

    #[test]
    fn a_goal_round_trips_through_its_stored_form() {
        let goal = ConversationGoal::from_codex(&json!({
            "threadId": "t", "objective": "o", "status": "paused"
        }))
        .unwrap();
        let stored = serde_json::to_string(&goal).unwrap();
        assert!(stored.contains("\"status\":\"paused\""), "{stored}");
        assert_eq!(
            serde_json::from_str::<ConversationGoal>(&stored).unwrap(),
            goal
        );
        assert_eq!(goal.usage_text(), None);
    }
}
