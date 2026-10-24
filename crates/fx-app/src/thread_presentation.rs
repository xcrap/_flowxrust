//! Translate provider and conversation data into shared UI controls.

use fx_agent::conversation::{Conversation, Phase};
use fx_core::{Agent, AgentExecutionState};
use fx_design::components::{ActivityState, ContextUsage, ProviderIdentity};

use crate::AppState;

pub(crate) fn provider(state: &AppState, agent: &Agent) -> ProviderIdentity {
    let native = state.native_defaults(agent.id);
    match agent
        .configuration
        .provider_id
        .as_deref()
        .or(native.provider_id.as_deref())
    {
        Some("claude") => ProviderIdentity::Claude,
        _ => ProviderIdentity::Codex,
    }
}

pub(crate) fn context_usage(
    state: &AppState,
    agent: &Agent,
    conversation: Option<&Conversation>,
) -> ContextUsage {
    let effective =
        crate::configuration::with_native_defaults(agent.clone(), state.native_defaults(agent.id));
    let provider = effective
        .configuration
        .provider_id
        .as_deref()
        .unwrap_or("codex");
    let model = effective
        .configuration
        .model_id
        .as_deref()
        .unwrap_or(if provider == "claude" {
            "default"
        } else {
            fx_core::DEFAULT_CODEX_MODEL_ID
        });
    let usage = conversation.map(Conversation::usage).unwrap_or_default();
    let limit = usage
        .reported_context_window
        .filter(|limit| *limit > 0)
        .or(effective
            .configuration
            .context_window_size
            .filter(|limit| *limit > 0))
        .or_else(|| {
            crate::provider_catalog::known_model(provider, model)
                .map(|model| model.context_window)
                .filter(|limit| *limit > 0)
        });
    // Existing history without telemetry is unknown, not an empty context.
    let known = usage.current_context_tokens > 0
        || usage.reported_context_window.is_some()
        || conversation.is_some_and(|conversation| conversation.message_count() == 0);
    ContextUsage {
        used: known.then_some(usage.current_context_tokens),
        limit,
    }
}

/// The status a sidebar row shows.
///
/// A settled thread reads its saved outcome whether or not its conversation is
/// loaded. Deciding from the loaded history instead made a row change from a
/// dot to a checkmark the moment it was clicked, with nothing having happened.
pub(crate) fn activity(
    phase: Option<Phase>,
    busy: bool,
    saved: AgentExecutionState,
) -> (ActivityState, &'static str) {
    match phase {
        Some(Phase::WaitingForApproval) => (ActivityState::Waiting, "Waiting for approval"),
        Some(Phase::WaitingForInput) => (ActivityState::Waiting, "Waiting for input"),
        Some(Phase::Failed) => (ActivityState::Error, "Failed"),
        Some(Phase::Starting) => (ActivityState::Running, "Starting"),
        Some(Phase::Compacting) => (ActivityState::Running, "Compacting context"),
        Some(Phase::Cancelling) => (ActivityState::Running, "Stopping"),
        Some(Phase::Running) => (ActivityState::Running, "Working"),
        _ if busy => (ActivityState::Running, "Starting"),
        Some(Phase::Idle) | None => match saved {
            AgentExecutionState::Success => (ActivityState::Completed, "Ready"),
            AgentExecutionState::Failure => (ActivityState::Error, "Failed"),
            // Nothing is running, so a saved in-flight state outlived the
            // process that ran it.
            AgentExecutionState::Running
            | AgentExecutionState::WaitingForApproval
            | AgentExecutionState::Idle => (ActivityState::Idle, "Idle"),
        },
    }
}

/// What a finished turn leaves as the thread's saved outcome.
pub(crate) fn settled_state(outcome: &fx_agent::codex::TurnOutcome) -> AgentExecutionState {
    match outcome {
        fx_agent::codex::TurnOutcome::Completed { .. } => AgentExecutionState::Success,
        fx_agent::codex::TurnOutcome::Failed { .. } => AgentExecutionState::Failure,
        fx_agent::codex::TurnOutcome::Cancelled { .. } => AgentExecutionState::Idle,
    }
}

/// Whether a finished turn is called out until its thread is opened.
///
/// Only a completed turn the reader was not looking at. A failure keeps its
/// own mark in the status tile for as long as it is the saved outcome, and a
/// cancelled turn was stopped by the reader, who has nothing new to see.
pub(crate) fn completion_needs_attention(
    outcome: &fx_agent::codex::TurnOutcome,
    selected: bool,
) -> bool {
    !selected && matches!(outcome, fx_agent::codex::TurnOutcome::Completed { .. })
}

/// Whether a row shows the unseen-completion dot beside its status tile.
///
/// The dot qualifies the tile's checkmark — finished, and not opened since —
/// so it is drawn only while the tile shows that outcome. A thread that starts
/// another turn, or whose saved outcome is gone, shows the tile alone.
pub(crate) fn shows_unseen_completion(
    unseen: bool,
    selected: bool,
    activity: ActivityState,
) -> bool {
    unseen && !selected && activity == ActivityState::Completed
}

/// The provider's own id for a thread, when it has one yet.
///
/// The loaded conversation is preferred because it learns a new session's id
/// the moment the turn starts; the binding covers threads never opened here.
pub(crate) fn provider_thread_id(state: &AppState, agent_id: uuid::Uuid) -> Option<String> {
    state
        .conversation_by_agent(agent_id)
        .and_then(|conversation| {
            conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .session_id()
                .map(str::to_owned)
        })
        .or_else(|| {
            state
                .snapshot
                .projects
                .iter()
                .flat_map(|record| &record.native_bindings)
                .find(|binding| binding.agent_id == agent_id)
                .map(|binding| binding.session_id.clone())
        })
        .filter(|id| !id.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[test]
    fn context_prefers_provider_telemetry_and_never_uses_cumulative_usage_as_fill() {
        let temp = tempfile::TempDir::new().unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(fx_store::AppPaths::at(temp.path()), cx));
            let mut agent = Agent::new("Context test");
            agent.configuration.context_window_size = Some(200_000);
            let mut conversation = Conversation::new(agent.id);
            assert_eq!(
                context_usage(state.read(cx), &agent, Some(&conversation)),
                ContextUsage {
                    used: Some(0),
                    limit: Some(200_000)
                }
            );
            conversation.append(fx_core::ConversationMessage::text(
                fx_core::MessageRole::User,
                "Prompt",
            ));
            conversation.record_usage(fx_agent::provider::Usage {
                input_tokens: 900_000,
                total_tokens: 900_000,
                ..Default::default()
            });
            assert_eq!(
                context_usage(state.read(cx), &agent, Some(&conversation)).used,
                None
            );
            conversation.record_context(25_000, 100_000);
            let usage = context_usage(state.read(cx), &agent, Some(&conversation));
            assert_eq!(
                usage,
                ContextUsage {
                    used: Some(25_000),
                    limit: Some(100_000)
                }
            );
            assert_eq!(usage.fraction(), Some(0.25));
        });
    }

    #[test]
    fn waiting_and_failure_are_not_hidden_by_a_live_runner() {
        for (phase, expected) in [
            (Phase::WaitingForApproval, ActivityState::Waiting),
            (Phase::WaitingForInput, ActivityState::Waiting),
            (Phase::Failed, ActivityState::Error),
        ] {
            assert_eq!(
                activity(Some(phase), true, AgentExecutionState::Idle).0,
                expected
            );
        }
    }

    #[test]
    fn startup_and_ready_states_follow_the_conversation_lifecycle() {
        assert_eq!(
            activity(Some(Phase::Idle), false, AgentExecutionState::Idle),
            (ActivityState::Idle, "Idle")
        );
        assert_eq!(
            activity(Some(Phase::Idle), true, AgentExecutionState::Idle),
            (ActivityState::Running, "Starting")
        );
        assert_eq!(
            activity(Some(Phase::Running), true, AgentExecutionState::Success),
            (ActivityState::Running, "Working")
        );
        assert_eq!(
            activity(Some(Phase::Idle), false, AgentExecutionState::Success),
            (ActivityState::Completed, "Ready")
        );
    }

    #[test]
    fn a_settled_thread_shows_the_same_status_loaded_or_not() {
        for saved in AgentExecutionState::ALL {
            assert_eq!(
                activity(Some(Phase::Idle), false, saved),
                activity(None, false, saved),
                "{saved:?}"
            );
        }
        // A saved in-flight state is stale once nothing is running.
        assert_eq!(
            activity(None, false, AgentExecutionState::Running).0,
            ActivityState::Idle
        );
    }

    #[test]
    fn only_a_completed_turn_away_from_the_reader_needs_attention() {
        use fx_agent::codex::TurnOutcome;
        let completed = TurnOutcome::Completed {
            thread_id: "thread".into(),
            stop_reason: "end_turn".into(),
            events: Vec::new(),
        };
        assert!(completion_needs_attention(&completed, false));
        assert!(!completion_needs_attention(&completed, true));
        for outcome in [
            TurnOutcome::Failed {
                thread_id: "thread".into(),
                reason: "boom".into(),
                events: Vec::new(),
            },
            TurnOutcome::Cancelled {
                thread_id: "thread".into(),
                events: Vec::new(),
            },
        ] {
            assert!(!completion_needs_attention(&outcome, false), "{outcome:?}");
        }
    }

    #[test]
    fn the_unseen_dot_only_qualifies_a_completed_tile_on_another_row() {
        assert!(shows_unseen_completion(
            true,
            false,
            ActivityState::Completed
        ));
        assert!(!shows_unseen_completion(
            true,
            true,
            ActivityState::Completed
        ));
        assert!(!shows_unseen_completion(
            false,
            false,
            ActivityState::Completed
        ));
        for activity in ActivityState::ALL {
            if activity != ActivityState::Completed {
                assert!(
                    !shows_unseen_completion(true, false, activity),
                    "{activity:?}"
                );
            }
        }
    }
}
