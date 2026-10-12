//! Splitting a turn into what the user reads and what they can expand.
//!
//! A turn arrives as prose interleaved with tool traffic: the model says what
//! it is about to do, calls a tool, gets a result, says what it found. Only the
//! prose is worth reading unprompted, and only the tool traffic is worth
//! collapsing behind a summary. Deciding which is which here means the view
//! receives runs already labelled, instead of re-deriving that per row.
//!
//! Two properties make the decision cheap to reason about:
//!
//! - It is per message first, then per *contiguous run*. A whole-turn group
//!   would swallow the prose in the middle of it.
//! - Role is deliberately not part of it. The caller splits the user's own
//!   turns out of the assistant's before calling, so within one call every
//!   message belongs to the same turn and the role carries no information the
//!   policy could act on. Re-deciding it here would only give a second, weaker
//!   place for the same rule to be wrong.

use crate::models::{ConversationMessage, MessageContent};

/// The tool the model calls to put a question to the user.
pub const QUESTION_TOOL_NAME: &str = "AskUserQuestion";

/// One contiguous run of messages, tagged with how it should be rendered.
///
/// The run borrows the transcript rather than owning a copy of it. A message
/// can carry image bytes inline, so an owned segment would mean deep-copying
/// every attachment in the turn on every render pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptActivitySegment<'a> {
    /// Prose, rendered as written.
    Narrative { messages: &'a [ConversationMessage] },
    /// Tool traffic, collapsed into a group the user can expand.
    Activity { messages: &'a [ConversationMessage] },
}

impl<'a> TranscriptActivitySegment<'a> {
    /// The messages in the run, whichever kind of run it is.
    pub fn messages(&self) -> &'a [ConversationMessage] {
        match self {
            Self::Narrative { messages } | Self::Activity { messages } => messages,
        }
    }
}

pub struct TranscriptActivityGroupingPolicy;

impl TranscriptActivityGroupingPolicy {
    /// Split a turn into runs, each labelled by how it should render.
    pub fn segments(messages: &[ConversationMessage]) -> Vec<TranscriptActivitySegment<'_>> {
        let mut segments = Vec::new();
        // The run being accumulated, as a range. A run cannot be closed until
        // the next message's classification is known, which is the only reason
        // this is a buffer rather than a push per message.
        let mut start = 0;
        let mut run_is_activity = false;

        for (index, message) in messages.iter().enumerate() {
            let is_activity = Self::is_activity_only(message);
            // Contiguity is the rule, not membership: the same tool traffic
            // either side of a sentence is two groups, because one group
            // spanning both would hide that sentence. It is also what gives the
            // caller one group per burst to drive its active state from.
            if index > start && is_activity != run_is_activity {
                segments.push(Self::segment(&messages[start..index], run_is_activity));
                start = index;
            }
            run_is_activity = is_activity;
        }

        if !messages.is_empty() {
            segments.push(Self::segment(&messages[start..], run_is_activity));
        }

        segments
    }

    /// Whether this message is nothing but background tool traffic.
    pub fn is_activity_only(message: &ConversationMessage) -> bool {
        // A message with no blocks has nothing to group, and `all` over an
        // empty iterator is vacuously true — so without this the transcript
        // grows a group header that describes no work at all.
        !message.content.is_empty()
            && message.content.iter().all(|block| match block {
                MessageContent::ToolUse { name, .. } => {
                    !name.eq_ignore_ascii_case(QUESTION_TOOL_NAME)
                }
                // A result has no meaning on its own: it is the payload of a
                // call, and becomes readable as a row inside the group that
                // shows what was called.
                MessageContent::ToolResult { .. } => true,
                // Any prose at all — even in the same message as a tool call —
                // keeps the message narrative. Collapsing the two together
                // would bury the words the model just said.
                MessageContent::Text { .. } | MessageContent::Code { .. } => false,
                // An image is content the turn is showing the user, not
                // bookkeeping about the turn.
                MessageContent::Image { .. } | MessageContent::ImageAsset(_) => false,
            })
    }

    fn segment(
        messages: &[ConversationMessage],
        is_activity: bool,
    ) -> TranscriptActivitySegment<'_> {
        if is_activity {
            TranscriptActivitySegment::Activity { messages }
        } else {
            TranscriptActivitySegment::Narrative { messages }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ConversationImageAssetReference, MessageRole};
    use uuid::Uuid;

    fn message(role: MessageRole, block: MessageContent) -> ConversationMessage {
        ConversationMessage::new(role, vec![block])
    }

    fn tool_use(role: MessageRole, id: &str, name: &str) -> ConversationMessage {
        message(
            role,
            MessageContent::ToolUse {
                id: id.into(),
                name: name.into(),
                input: "{}".into(),
            },
        )
    }

    fn tool_result(id: &str, content: &str) -> ConversationMessage {
        message(
            MessageRole::Tool,
            MessageContent::ToolResult {
                id: id.into(),
                content: content.into(),
                is_error: false,
            },
        )
    }

    #[test]
    fn assistant_narrative_remains_outside_tool_groups_in_chronological_order() {
        let opening = ConversationMessage::text(
            MessageRole::Assistant,
            "I'll inspect the current implementation.",
        );
        let read = tool_use(MessageRole::Assistant, "read", "Read");
        let read_result = tool_result("read", "source");
        let update = ConversationMessage::text(
            MessageRole::Assistant,
            "I found the issue and am applying the fix.",
        );
        let edit = tool_use(MessageRole::Assistant, "edit", "Edit");
        let edit_result = tool_result("edit", "updated");
        let completion = ConversationMessage::text(MessageRole::Assistant, "The fix is complete.");

        let source = vec![
            opening,
            read,
            read_result,
            update,
            edit,
            edit_result,
            completion,
        ];

        let segments = TranscriptActivityGroupingPolicy::segments(&source);

        assert_eq!(
            segments,
            vec![
                TranscriptActivitySegment::Narrative {
                    messages: &source[0..1]
                },
                TranscriptActivitySegment::Activity {
                    messages: &source[1..3]
                },
                TranscriptActivitySegment::Narrative {
                    messages: &source[3..4]
                },
                TranscriptActivitySegment::Activity {
                    messages: &source[4..6]
                },
                TranscriptActivitySegment::Narrative {
                    messages: &source[6..7]
                },
            ]
        );
        assert_eq!(
            segments
                .iter()
                .flat_map(|segment| segment.messages().iter().cloned())
                .collect::<Vec<_>>(),
            source,
            "the segments must be a partition of the input, in order"
        );
    }

    #[test]
    fn mixed_narrative_and_tool_content_is_never_hidden_as_activity() {
        let mixed = ConversationMessage::new(
            MessageRole::Assistant,
            vec![
                MessageContent::Text {
                    text: "I am checking this now.".into(),
                },
                MessageContent::ToolUse {
                    id: "shell".into(),
                    name: "Bash".into(),
                    input: "{}".into(),
                },
            ],
        );
        let source = [mixed];

        assert_eq!(
            TranscriptActivityGroupingPolicy::segments(&source),
            vec![TranscriptActivitySegment::Narrative { messages: &source }]
        );
    }

    #[test]
    fn user_questions_remain_narrative_instead_of_background_activity() {
        let question = tool_use(MessageRole::Assistant, "question", QUESTION_TOOL_NAME);
        let source = [question];

        assert_eq!(
            TranscriptActivityGroupingPolicy::segments(&source),
            vec![TranscriptActivitySegment::Narrative { messages: &source }]
        );
    }

    #[test]
    fn a_question_and_its_answer_stay_out_of_the_group() {
        // Both halves matter for a different reason: the question before it is
        // answered, the answer after. Collapsing the pair hides both behind a
        // summary of a tool the user already finished with.
        let question = tool_use(MessageRole::Assistant, "question", QUESTION_TOOL_NAME);
        let answer = tool_result("question", "the second one");
        let source = vec![question, answer];

        assert_eq!(
            TranscriptActivityGroupingPolicy::segments(&source),
            vec![
                TranscriptActivitySegment::Narrative {
                    messages: &source[0..1]
                },
                TranscriptActivitySegment::Activity {
                    messages: &source[1..2]
                },
            ]
        );
    }

    #[test]
    fn the_question_tool_is_recognized_without_regard_to_casing() {
        // The name arrives from provider JSON, and the Swift original compared
        // it case-insensitively for that reason. A tool named
        // `RunAskUserQuestion` is a different tool and must still collapse.
        for name in ["AskUserQuestion", "askuserquestion", "ASKUSERQUESTION"] {
            let question = tool_use(MessageRole::Assistant, "question", name);
            assert!(
                !TranscriptActivityGroupingPolicy::is_activity_only(&question),
                "{name} was collapsed as background activity"
            );
        }

        let other = tool_use(MessageRole::Assistant, "q", "RunAskUserQuestion");
        assert!(
            TranscriptActivityGroupingPolicy::is_activity_only(&other),
            "a name that merely contains the question tool is still a tool call"
        );
    }

    #[test]
    fn only_tool_blocks_classify_a_message_as_activity() {
        let cases = [
            (MessageContent::Text { text: "hi".into() }, false),
            (
                MessageContent::Code {
                    language: "rust".into(),
                    code: "fn main() {}".into(),
                },
                false,
            ),
            (
                MessageContent::Image {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                },
                false,
            ),
            (
                MessageContent::ImageAsset(ConversationImageAssetReference {
                    project_id: Uuid::nil(),
                    agent_id: Uuid::nil(),
                    message_id: Uuid::nil(),
                    content_index: 0,
                    mime_type: "image/png".into(),
                    byte_count: 3,
                }),
                false,
            ),
            (
                MessageContent::ToolUse {
                    id: "t".into(),
                    name: "Read".into(),
                    input: "{}".into(),
                },
                true,
            ),
            (
                MessageContent::ToolResult {
                    id: "t".into(),
                    content: "ok".into(),
                    is_error: false,
                },
                true,
            ),
        ];

        for (block, expected) in cases {
            let message = ConversationMessage::new(MessageRole::Assistant, vec![block.clone()]);
            assert_eq!(
                TranscriptActivityGroupingPolicy::is_activity_only(&message),
                expected,
                "{block:?} was misclassified"
            );
        }
    }

    #[test]
    fn consecutive_tool_traffic_is_a_single_group() {
        let call = tool_use(MessageRole::Assistant, "read", "Read");
        let result = tool_result("read", "source");
        let failure = ConversationMessage::new(
            MessageRole::Tool,
            vec![MessageContent::ToolResult {
                id: "write".into(),
                content: "permission denied".into(),
                is_error: true,
            }],
        );
        // An error result is still activity: reporting which call failed is
        // exactly what the group is for.
        let source = vec![call, result, failure];

        let segments = TranscriptActivityGroupingPolicy::segments(&source);

        assert_eq!(segments.len(), 1, "no group was broken mid-run");
        assert_eq!(
            segments[0],
            TranscriptActivitySegment::Activity {
                messages: &source[..]
            }
        );
    }

    #[test]
    fn a_message_with_no_blocks_is_narrative_rather_than_an_empty_group() {
        let empty = ConversationMessage::new(MessageRole::Assistant, vec![]);
        let source = [empty];

        assert!(!TranscriptActivityGroupingPolicy::is_activity_only(
            &source[0]
        ));
        assert_eq!(
            TranscriptActivityGroupingPolicy::segments(&source),
            vec![TranscriptActivitySegment::Narrative { messages: &source }]
        );
    }

    #[test]
    fn an_empty_transcript_has_no_segments() {
        assert!(TranscriptActivityGroupingPolicy::segments(&[]).is_empty());
    }

    #[test]
    fn a_run_of_one_is_still_a_run() {
        // The single-tool-call turn is the common case, and it is the one where
        // a grouping bug shows as a group with no rows.
        let call = tool_use(MessageRole::Assistant, "list", "LS");
        let source = [call];

        assert_eq!(
            TranscriptActivityGroupingPolicy::segments(&source),
            vec![TranscriptActivitySegment::Activity { messages: &source }]
        );
    }

    #[test]
    fn the_role_does_not_decide_how_a_message_is_grouped() {
        // Documented consequence of leaving role out. The caller splits the
        // user's own turns off before grouping, so a `user`-role message
        // reaching this policy is not the case the rule was about — and
        // deciding it here as well would be a second place for it to be wrong.
        let result = ConversationMessage::new(
            MessageRole::User,
            vec![MessageContent::ToolResult {
                id: "read".into(),
                content: "source".into(),
                is_error: false,
            }],
        );

        assert!(TranscriptActivityGroupingPolicy::is_activity_only(&result));
    }
}
