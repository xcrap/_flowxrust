//! Search saved and live messages on a worker; never read a transcript in a
//! render. Stale searches are cancelled and cannot replace newer results.

use crate::{state::AppState, text_field::TextField};
use fx_core::{ConversationMessage, policy::message_search::MessageQuery};
use fx_design::{
    components::{IconButtonProps, icon_button},
    icon::Icon,
    styled::{FxStyled, px_len},
    theme::{Colors, Spacing, global},
};
use gpui::{
    App, Context, Entity, Render, SharedString, Subscription, Task, Window, div, prelude::*,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use uuid::Uuid;

pub(crate) struct Source {
    pub agent: Uuid,
    pub project: Uuid,
    pub title: String,
    pub project_name: String,
    pub live: Option<Vec<Arc<ConversationMessage>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub agent: Uuid,
    pub message: Uuid,
    pub title: String,
    pub project_name: String,
    pub snippet: String,
}

#[derive(Default)]
pub(crate) struct Results {
    pub hits: Vec<Hit>,
    pub unavailable: usize,
    pub capped: bool,
}

pub(crate) struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn sources(state: &AppState, only: Option<Uuid>) -> Vec<Source> {
    state
        .snapshot
        .projects
        .iter()
        .flat_map(|record| {
            record.agents.iter().filter_map(|agent| {
                let project = record.project.as_ref()?;
                if only.is_some_and(|id| agent.id != id)
                    || record.archived_agent_ids.contains(&agent.id)
                    || record.missing_native_agent_ids.contains(&agent.id)
                {
                    return None;
                }
                let live = state
                    .conversations
                    .get(&agent.id)
                    .filter(|_| {
                        !state.is_loading_conversation(agent.id)
                            && !state.has_unreadable_conversation(agent.id)
                    })
                    .map(|conversation| {
                        conversation
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .shared_messages()
                            .cloned()
                            .collect()
                    });
                Some(Source {
                    agent: agent.id,
                    project: project.id,
                    title: agent.title.clone(),
                    project_name: project.name.clone(),
                    live,
                })
            })
        })
        .collect()
}

pub(crate) fn search(
    store: fx_store::Store,
    sources: Vec<Source>,
    query: String,
    cancelled: Arc<AtomicBool>,
    limit: usize,
) -> Results {
    let query = MessageQuery::new(&query);
    let mut result = Results::default();
    if query.is_empty() {
        return result;
    }
    for source in sources {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let messages = match source.live {
            Some(messages) => messages,
            None => match store.load_conversation(source.project, source.agent) {
                Ok(Some(saved)) => saved.messages.into_iter().map(Arc::new).collect(),
                Ok(None) => {
                    result.unavailable += 1;
                    continue;
                }
                Err(_) => {
                    result.unavailable += 1;
                    continue;
                }
            },
        };
        for message in messages {
            if cancelled.load(Ordering::Relaxed) {
                return result;
            }
            if let Some(snippet) = query.snippet(&message) {
                if result.hits.len() == limit {
                    result.capped = true;
                    return result;
                }
                result.hits.push(Hit {
                    agent: source.agent,
                    message: message.id,
                    title: source.title.clone(),
                    project_name: source.project_name.clone(),
                    snippet,
                });
            }
        }
    }
    result
}

#[derive(Clone, Copy)]
pub(crate) struct Target {
    pub agent: Uuid,
    pub message: Uuid,
    pub pending: bool,
}

pub(crate) fn reveal(state: &Entity<AppState>, agent: Uuid, message: Uuid, cx: &mut App) {
    state.update(cx, |state, cx| {
        if state.snapshot.active_agent_id != Some(agent) {
            state.open_thread(agent, cx);
        }
        if state.snapshot.active_agent_id == Some(agent) {
            state.search_target = Some(Target {
                agent,
                message,
                pending: true,
            });
        }
        cx.notify();
    });
}

pub(crate) struct ConversationFind {
    state: Entity<AppState>,
    pub agent: Uuid,
    query: Entity<TextField>,
    results: Results,
    current: usize,
    searching: bool,
    generation: u64,
    searched_query: String,
    revision: Option<u64>,
    task: Option<Task<()>>,
    cancellation: Option<Cancellation>,
    _query_changed: Subscription,
    _state_changed: Subscription,
}

pub(crate) fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let Some(agent) = state.read(cx).snapshot.active_agent_id else {
        return;
    };
    if let Some(find) = &state.read(cx).find
        && find.read(cx).agent == agent
    {
        find.read(cx).query.read(cx).focus_handle().focus(window);
        return;
    }
    let find = cx.new(|cx| {
        let query = cx.new(|cx| {
            TextField::new(cx)
                .bare()
                .placeholder("Find in this conversation…")
                .max_len(200)
        });
        let query_changed = cx.observe(&query, |this: &mut ConversationFind, _, cx| {
            if this.query.read(cx).value().trim() != this.searched_query {
                this.refresh(cx);
            }
        });
        let state_changed = cx.observe(&state, |this: &mut ConversationFind, _, cx| {
            if this.state.read(cx).snapshot.active_agent_id != Some(this.agent) {
                this.task = None;
                this.cancellation = None;
                let owner = cx.entity();
                this.state.update(cx, |state, cx| {
                    if state.find.as_ref() == Some(&owner) {
                        state.find = None;
                        if state
                            .search_target
                            .is_some_and(|target| target.agent == this.agent)
                        {
                            state.search_target = None;
                        }
                        cx.notify();
                    }
                });
                return;
            }
            let revision = this.state.read(cx).conversations.get(&this.agent).map(|c| {
                c.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .message_revision()
            });
            if this.revision != revision {
                this.revision = revision;
                this.refresh(cx);
            }
        });
        ConversationFind {
            state: state.clone(),
            agent,
            query,
            results: Results::default(),
            current: 0,
            searching: false,
            generation: 0,
            searched_query: String::new(),
            revision: None,
            task: None,
            cancellation: None,
            _query_changed: query_changed,
            _state_changed: state_changed,
        }
    });
    find.read(cx).query.read(cx).focus_handle().focus(window);
    state.update(cx, |state, cx| {
        state.find = Some(find);
        state.search_target = None;
        cx.notify();
    });
}

impl ConversationFind {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.task = None;
        self.cancellation = None;
        let query = self.query.read(cx).value().trim().to_string();
        if query != self.searched_query {
            self.results = Results::default();
            self.current = 0;
            self.state.update(cx, |state, cx| {
                state.search_target = None;
                cx.notify();
            });
        }
        self.searched_query = query.clone();
        self.searching = !query.is_empty();
        if !self.searching {
            cx.notify();
            return;
        }
        let generation = self.generation;
        let cancellation = Cancellation::new();
        let flag = cancellation.flag();
        self.cancellation = Some(cancellation);
        let state = self.state.clone();
        let agent = self.agent;
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(180))
                .await;
            let Ok((store, sources)) = cx.read_entity(&state, |state, _| {
                (state.store.clone(), sources(state, Some(agent)))
            }) else {
                return;
            };
            let results = cx
                .background_executor()
                .spawn(async move { search(store, sources, query, flag, 10_000) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                // Read the current hit when the results land: the user may
                // have navigated while this refresh was running.
                let selected = this.results.hits.get(this.current).map(|hit| hit.message);
                this.current = selected
                    .and_then(|id| results.hits.iter().position(|hit| hit.message == id))
                    .unwrap_or_else(|| this.current.min(results.hits.len().saturating_sub(1)));
                this.results = results;
                this.searching = false;
                if this.results.hits.get(this.current).map(|hit| hit.message) != selected {
                    this.show_current(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn show_current(&self, cx: &mut Context<Self>) {
        if self.state.read(cx).snapshot.active_agent_id != Some(self.agent) {
            return;
        }
        if let Some(hit) = self.results.hits.get(self.current) {
            reveal(&self.state, hit.agent, hit.message, cx);
        } else {
            self.state.update(cx, |state, cx| {
                state.search_target = None;
                cx.notify();
            });
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.results.hits.is_empty() {
            return;
        }
        self.current =
            (self.current as isize + delta).rem_euclid(self.results.hits.len() as isize) as usize;
        self.show_current(cx);
        cx.notify();
    }

    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.state.update(cx, |state, cx| {
            state.find = None;
            state.search_target = None;
            cx.notify();
            state.drafts.get(&self.agent).cloned()
        });
        if let Some(draft) = draft {
            draft.read(cx).focus_handle().focus(window);
        }
    }
}

impl Render for ConversationFind {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = global::current();
        let count = self.results.hits.len();
        let status = if self.searching {
            "Searching…".into()
        } else if self.query.read(cx).value().trim().is_empty() {
            "".into()
        } else {
            format!(
                "{} of {count}{} messages",
                if count == 0 { 0 } else { self.current + 1 },
                if self.results.capped { "+" } else { "" }
            )
        };
        div()
            .id("conversation-find")
            .debug_selector(|| "conversation-find".into())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(Colors::bg_elevated(&theme))
            .px(px_len(Spacing::MD))
            .py(px_len(Spacing::XS))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if this.query.read(cx).is_composing()
                    || modifiers.platform
                    || modifiers.control
                    || modifiers.alt
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "enter" => {
                        this.step(
                            if event.keystroke.modifiers.shift {
                                -1
                            } else {
                                1
                            },
                            cx,
                        );
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.close(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px_len(Spacing::XS))
                    .child(div().flex_1().min_w_0().child(self.query.clone()))
                    .child(
                        div()
                            .fx_caption()
                            .text_color(Colors::fg_secondary(&theme))
                            .child(SharedString::from(status)),
                    )
                    .child(icon_button(
                        IconButtonProps::new(Icon::CHEVRON_UP, "Previous matching message")
                            .small()
                            .ghost()
                            .enabled(count > 0),
                        cx.listener(|this, _, _, cx| this.step(-1, cx)),
                    ))
                    .child(icon_button(
                        IconButtonProps::new(Icon::CHEVRON_DOWN, "Next matching message")
                            .small()
                            .ghost()
                            .enabled(count > 0),
                        cx.listener(|this, _, _, cx| this.step(1, cx)),
                    ))
                    .child(icon_button(
                        IconButtonProps::new(Icon::XMARK, "Close search")
                            .small()
                            .ghost(),
                        cx.listener(|this, _, window, cx| this.close(window, cx)),
                    )),
            )
            .children(self.results.hits.get(self.current).map(|hit| {
                div()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child(SharedString::from(hit.snippet.clone()))
            }))
            .children((self.results.unavailable > 0).then(|| {
                div()
                    .fx_caption()
                    .text_color(Colors::fg_secondary(&theme))
                    .child("Saved history is unavailable. Check this conversation's loading error.")
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::MessageRole;
    use fx_store::{AppPaths, PersistedConversation, Store};

    #[test]
    fn search_reads_unopened_caches_prefers_live_text_and_never_writes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(AppPaths::at(temp.path().join("state")));
        let project = Uuid::new_v4();
        let agents = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let saved = ConversationMessage::text(MessageRole::Assistant, "Açores saved answer");
        let live = ConversationMessage::text(MessageRole::User, "Açores live prompt");
        for agent in &agents[..2] {
            let mut record = PersistedConversation::from_messages(*agent, vec![saved.clone()]);
            record.draft_text = "secret unsent draft".into();
            store.save_conversation(project, *agent, &record).unwrap();
        }
        let sources = || {
            agents
                .iter()
                .enumerate()
                .map(|(i, agent)| Source {
                    agent: *agent,
                    project,
                    title: format!("Chat {i}"),
                    project_name: "Project".into(),
                    live: (i == 1).then(|| vec![Arc::new(live.clone())]),
                })
                .collect()
        };
        let cancel = Cancellation::new();
        let results = search(
            store.clone(),
            sources(),
            "AÇORES".into(),
            cancel.flag(),
            100,
        );
        assert_eq!(
            results
                .hits
                .iter()
                .map(|hit| (hit.agent, hit.message))
                .collect::<Vec<_>>(),
            [(agents[0], saved.id), (agents[1], live.id)]
        );
        assert_eq!(results.unavailable, 1);
        assert!(!results.capped);
        let capped = search(store.clone(), sources(), "açores".into(), cancel.flag(), 1);
        assert!(capped.capped);
        assert_eq!(capped.hits.len(), 1);
        assert!(
            search(
                store.clone(),
                sources(),
                "secret".into(),
                cancel.flag(),
                100
            )
            .hits
            .is_empty()
        );
        assert_eq!(
            store
                .load_conversation(project, agents[1])
                .unwrap()
                .unwrap()
                .messages[0]
                .text_content(),
            saved.text_content()
        );
        let flag = cancel.flag();
        drop(cancel);
        assert!(
            search(store, sources(), "açores".into(), flag, 100)
                .hits
                .is_empty()
        );
    }
}
