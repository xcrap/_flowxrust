//! User-selected context enters the ordinary draft as an undoable, editable
//! block. Its existing persistence, queue, steer and retry paths carry it intact.

use crate::state::AppState;
use gpui::{App, Entity, Window};
use uuid::Uuid;

pub(crate) fn add(
    state: &Entity<AppState>,
    agent: Uuid,
    text: &str,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let field = state.update(cx, |state, cx| state.append_prompt_context(agent, text, cx));
    if let Some(field) = field {
        field.read(cx).focus_handle().focus(window);
        true
    } else {
        false
    }
}

pub(crate) fn pick_files(state: &Entity<AppState>, agent: Uuid, cx: &mut App) {
    let ticket = Uuid::new_v4();
    state.update(cx, |state, _| {
        state.composer.reference_picks.insert(agent, ticket);
    });
    let chosen = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Reference".into()),
    });
    let state = state.clone();
    cx.spawn(async move |cx| {
        let paths = chosen.await;
        let _ = state.update(cx, |state, cx| {
            if state.composer.reference_picks.get(&agent) != Some(&ticket) {
                return;
            }
            state.composer.reference_picks.remove(&agent);
            if let Ok(Ok(Some(paths))) = paths {
                let text = paths
                    .iter()
                    .map(|path| {
                        fx_core::policy::prompt_context::file_reference(&path.to_string_lossy())
                    })
                    .collect::<String>();
                if !text.is_empty() {
                    state.append_prompt_context(agent, &text, cx);
                }
            }
        });
    })
    .detach();
}
