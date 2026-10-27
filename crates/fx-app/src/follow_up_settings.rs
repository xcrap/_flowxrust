//! Reversible setting for what a prompt sent while a turn runs does.
//!
//! The original's "While responding" row: steering guides the running turn
//! at once, queueing holds the prompt until the turn ends. ⌘↩ does the chosen
//! one and ⌃↩ the other, so the setting picks a default rather than removing
//! a choice.

use fx_design::components::segmented::{SegmentedOption, segmented_control};
use fx_design::components::{settings_group, settings_row, settings_section};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Render, Window, div};

use crate::state::FollowUp;

pub(crate) struct FollowUpSettings {
    draft: FollowUp,
    baseline: FollowUp,
}

impl FollowUpSettings {
    pub(crate) fn new(current: FollowUp, cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            draft: current,
            baseline: current,
        })
    }

    pub(crate) fn value(&self) -> FollowUp {
        self.draft
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.draft != self.baseline
    }

    pub(crate) fn reset(&mut self, cx: &mut Context<Self>) {
        self.draft = self.baseline;
        cx.notify();
    }

    pub(crate) fn set(&mut self, mode: FollowUp, cx: &mut Context<Self>) {
        self.draft = mode;
        cx.notify();
    }
}

/// What each choice does, in the original's words.
fn explanation(mode: FollowUp) -> &'static str {
    match mode {
        FollowUp::Steer => "Guide the active run immediately. Control-Return queues instead.",
        FollowUp::Queue => "Run after the current turn. Control-Return steers instead.",
    }
}

impl Render for FollowUpSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        settings_section(
            "While responding",
            Some("What a prompt sent during a turn does.".into()),
            settings_group([settings_row(
                "Command-Return",
                Some(explanation(self.draft).into()),
                div()
                    .debug_selector(|| "settings-follow-up-mode".into())
                    .child(segmented_control(
                        &[
                            SegmentedOption::new(FollowUp::Steer, FollowUp::Steer.label()),
                            SegmentedOption::new(FollowUp::Queue, FollowUp::Queue.label()),
                        ],
                        &self.draft,
                        move |mode, _, cx| this.update(cx, |editor, cx| editor.set(*mode, cx)),
                    )),
            )
            .into_any_element()]),
        )
        .debug_selector(|| "settings-follow-up".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_is_reversible_until_applied() {
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let editor = FollowUpSettings::new(FollowUp::Steer, cx);
            editor.update(cx, |editor, cx| {
                assert!(!editor.is_dirty());
                editor.set(FollowUp::Queue, cx);
                assert!(editor.is_dirty());
                assert_eq!(editor.value(), FollowUp::Queue);
                editor.reset(cx);
                assert_eq!(editor.value(), FollowUp::Steer);
            });
        });
        assert_ne!(explanation(FollowUp::Steer), explanation(FollowUp::Queue));
    }
}
