//! Application state.
//!
//! Holds only what must outlive a redraw and what the views cannot derive for
//! themselves: the loaded snapshot, the current selection, the open menu, and
//! the design settings. Anything a view can compute from those stays in the
//! view.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A conversation that a background turn can write into while the UI reads it.
pub type GuardedConversation = Mutex<Conversation>;

use fx_agent::conversation::Conversation;
use fx_core::{Agent, AgentConfiguration, Project};
use fx_design::components::{ContextRegistry, MenuLayer};
use fx_design::theme::{
    AccentColor, AppearanceMode, BaseTone, Layout, TextSizePreset, Theme, global,
};
use fx_store::{AppPaths, PersistedApp, PersistedConversation, PersistedPreferences, Store};

use crate::text_field::TextField;
use gpui::{App, AppContext, Context, Entity};
use uuid::Uuid;

/// Store failures, as the shell sees them.
///
/// I/O problems are not fatal to the session: the app keeps running with an
/// empty in-memory model so the user can still work, and reports the failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Load(#[from] fx_store::LoadError),
    #[error(transparent)]
    Save(#[from] fx_store::SaveError),
}

/// Root state for the window.
pub struct AppState {
    pub store: Store,
    pub snapshot: PersistedApp,
    pub preferences: PersistedPreferences,
    title_jobs: HashMap<Uuid, crate::auto_title::TitleJob>,
    /// Live transcripts, keyed by agent. Populated as threads are opened.
    ///
    /// Guarded, because a turn runs on a background thread and writes events into
    /// the conversation while the UI reads it on every render. Unguarded would
    /// mean the render path could not borrow it at all.
    pub conversations: HashMap<Uuid, Arc<GuardedConversation>>,
    /// One draft field per thread, created on first use.
    ///
    /// Per thread rather than one shared field, because a draft is the user's
    /// unsent work: switching threads and back must not lose it.
    pub drafts: HashMap<Uuid, Entity<TextField>>,
    /// Each thread owns its running turn, including while another is selected.
    pub runners: HashMap<Uuid, crate::session::TurnRunner>,
    /// Updates from the running turn, delivered from its thread.
    ///
    /// A channel rather than a captured `App`: the runner finishes on a
    /// background thread and `App` is neither `Send` nor reachable from there.
    /// The UI drains this on each render, which is also what makes a finished
    /// turn repaint.
    updates: std::sync::mpsc::Receiver<crate::session::SessionUpdate>,
    /// The matching sender, for handing to a runner.
    update_tx: std::sync::mpsc::Sender<crate::session::SessionUpdate>,
    targeted_updates: std::sync::mpsc::Receiver<(Uuid, crate::session::SessionUpdate)>,
    targeted_tx: std::sync::mpsc::SyncSender<(Uuid, crate::session::SessionUpdate)>,
    /// A failed read must never be followed by an empty write over the source.
    snapshot_writable: bool,
    protected_conversations: HashSet<Uuid>,
    /// Late worker notifications must never recreate a deleted thread.
    deleted_agents: HashSet<Uuid>,
    restored_drafts: HashMap<Uuid, String>,
    /// Unacknowledged submissions to restore after startup fails or is cancelled.
    retry_drafts: HashMap<Uuid, String>,
    /// The save of a panel toggle, held until the panel has finished moving.
    panel_save: Option<gpui::Task<()>>,
    /// Claude threads whose context has been asked for this launch, so a
    /// failed or slow check is not repeated on every frame.
    context_probed: HashSet<Uuid>,
    /// The revision and draft each transcript was last handed to the writer at,
    /// or read from disk at. A version, never the bytes: the writer compares
    /// content by hash, so nothing here grows with the size of a transcript.
    saved_conversations: HashMap<Uuid, (u64, String)>,
    /// Encodes and writes transcripts on its own thread.
    writer: crate::conversation_writer::ConversationWriter,
    /// Threads whose transcript is being read in the background.
    loading: HashSet<Uuid>,
    restored_costs: HashMap<Uuid, f64>,
    last_checkpoint: Instant,
    /// Approvals and questions the reader has not answered yet. Answering one
    /// takes it out, so a second click cannot send the provider a second reply;
    /// its card stays until the provider resolves it.
    pending_interactions: HashMap<Uuid, VecDeque<fx_agent::ProviderEvent>>,
    /// The add-project sheet, while it is open.
    ///
    /// Owned here rather than by the sidebar so that the menu's "New Project"
    /// item and the sidebar's button open the same sheet, and so it can be
    /// drawn above the whole shell instead of inside one column.
    pub picker: Option<Entity<crate::project_picker::ProjectPicker>>,
    /// The command palette, while it is open. Held here for the same reasons as
    /// the picker: ⌘K and the View menu open the same one, above every column.
    pub palette: Option<Entity<crate::command_palette::CommandPalette>>,
    pub sidebar_visible: bool,
    /// The thread search and per-project paging, kept here so they survive the
    /// sidebar being hidden and shown again.
    pub sidebar: crate::sidebar::SidebarUi,
    /// The one menu that may be open, app-wide.
    pub menu_layer: Entity<MenuLayer>,
    /// Which screen regions offer a context menu.
    pub context_registry: Entity<ContextRegistry>,
    /// The last save or load failure, kept so the UI can say so.
    pub last_error: Option<String>,
    pub paths: AppPaths,
    native_sync: crate::native_sync::NativeSync,
    /// Images, queued images, steering and goals, per thread.
    pub composer: ComposerStore,
}

impl AppState {
    /// Build state from a store, restoring what was saved.
    ///
    /// Takes a `Context` because the menu layer and the context registry are
    /// entities: both are app-wide singletons, and creating them here rather
    /// than in each view is what keeps them single.
    ///
    /// Transcripts are restored lazily, when a thread is first shown, so launch
    /// does not parse every transcript in every project.
    pub fn restore(paths: AppPaths, cx: &mut Context<Self>) -> Self {
        let store = Store::new(paths.clone());
        let preferences = store.load_preferences().unwrap_or_else(|error| {
            tracing::warn!(%error, "could not restore preferences");
            PersistedPreferences::default()
        });
        let (mut snapshot, last_error, snapshot_writable) = match store.load_app() {
            Ok(snapshot) => (snapshot, None, true),
            Err(error) => {
                // Start empty rather than refusing to open, but keep the reason:
                // a blank sidebar that looks like "no projects" is a much worse
                // failure than one that says the file could not be read.
                tracing::error!(%error, "could not restore the project snapshot");
                (PersistedApp::default(), Some(error.to_string()), false)
            }
        };

        // Migrate the oversized port default while retaining custom widths.
        if snapshot.right_panel_width == 760.0 && snapshot.min_right_panel_width == 560.0 {
            snapshot.right_panel_width = Layout::RIGHT_PANEL_DEFAULT_WIDTH;
        }
        snapshot.min_right_panel_width = Layout::RIGHT_PANEL_MIN_WIDTH;
        snapshot.max_right_panel_width = Layout::RIGHT_PANEL_MAX_WIDTH;

        // Stale or archived selections never reopen a hidden thread.
        if !snapshot.projects.iter().any(|record| {
            record
                .project
                .as_ref()
                .is_some_and(|project| Some(project.id) == snapshot.active_project_id)
        }) {
            snapshot.active_project_id = snapshot
                .projects
                .iter()
                .find_map(|record| record.project.as_ref().map(|project| project.id));
        }
        snapshot.active_agent_id = snapshot
            .projects
            .iter()
            .find(|record| {
                record
                    .project
                    .as_ref()
                    .is_some_and(|project| Some(project.id) == snapshot.active_project_id)
            })
            .and_then(|record| {
                visible_agent(
                    record,
                    snapshot.active_agent_id.or(record.last_selected_agent_id),
                )
            });

        let sidebar_visible = snapshot.sidebar_visible;
        // Both halves of one channel: the runner sends, the render path receives.
        let (update_tx, updates) = std::sync::mpsc::channel();
        let (targeted_tx, targeted_updates) = std::sync::mpsc::sync_channel(256);
        Self {
            menu_layer: cx.new(|_| MenuLayer::default()),
            context_registry: cx.new(|_| ContextRegistry::default()),
            store,
            snapshot,
            preferences,
            title_jobs: HashMap::new(),
            conversations: HashMap::new(),
            drafts: HashMap::new(),
            runners: HashMap::new(),
            updates,
            update_tx,
            targeted_updates,
            targeted_tx,
            snapshot_writable,
            protected_conversations: HashSet::new(),
            deleted_agents: HashSet::new(),
            restored_drafts: HashMap::new(),
            retry_drafts: HashMap::new(),
            panel_save: None,
            context_probed: HashSet::new(),
            saved_conversations: HashMap::new(),
            writer: Default::default(),
            loading: HashSet::new(),
            restored_costs: HashMap::new(),
            last_checkpoint: Instant::now(),
            pending_interactions: HashMap::new(),
            picker: None,
            palette: None,
            sidebar_visible,
            sidebar: crate::sidebar::SidebarUi::new(cx),
            last_error,
            paths,
            native_sync: Default::default(),
            composer: ComposerStore::default(),
        }
    }

    /// Act on a menu row.
    ///
    /// Ids are the contract between `MenuModel` and this function, so they are
    /// matched here and nowhere else. An unknown id is ignored rather than
    /// logged loudly: the layer may hold a menu from a view that has since been
    /// replaced.
    pub fn apply_menu_choice(&mut self, owner: &str, chosen: &str) {
        match chosen {
            "sidebar" => self.toggle_sidebar(),
            "inspector" => self.toggle_inspector(),
            "new-thread" => {
                if let Some(id) = owner
                    .strip_prefix("project:")
                    .and_then(|id| id.parse().ok())
                {
                    self.select_project(id);
                }
                self.create_thread();
            }
            "collapse" => self.collapse_all_projects(),
            // Owner-scoped choices act on their own project; anything else in
            // the project menu is not implemented yet and is a no-op rather than
            // a lie.
            "remove" if owner.starts_with("project:") => {
                if let Ok(id) = owner["project:".len()..].parse() {
                    self.remove_project(id);
                }
            }
            _ => tracing::debug!(owner, chosen, "menu choice not implemented"),
        }
        self.save();
    }

    /// Collapse every project in the sidebar.
    pub fn collapse_all_projects(&mut self) {
        for record in &mut self.snapshot.projects {
            record.is_expanded = false;
        }
    }

    /// Create a thread in the active project and select it.
    ///
    /// Returns the new thread, or `None` when no project is active — there is
    /// nowhere to put a thread that has no project.
    pub fn create_thread(&mut self) -> Option<Uuid> {
        let project_id = self.snapshot.active_project_id?;
        let provider = self
            .active_agent()
            .and_then(|agent| {
                agent
                    .configuration
                    .provider_id
                    .or_else(|| self.native_defaults(agent.id).provider_id)
            })
            .unwrap_or_else(|| "codex".into());
        let mut agent = fx_core::Agent::new("Untitled thread");
        agent.configuration = self.default_configuration(&provider);

        let record = self
            .snapshot
            .projects
            .iter_mut()
            .find(|record| record.project.as_ref().is_some_and(|p| p.id == project_id))?;
        let project = record.project.as_mut()?;
        project.agent_order.push(agent.id);
        record.agents.push(agent.clone());
        record.auto_title_pending.push(agent.id);
        record.is_expanded = true;

        self.conversations
            .insert(agent.id, Arc::new(Mutex::new(Conversation::new(agent.id))));
        self.snapshot.active_agent_id = Some(agent.id);
        Some(agent.id)
    }

    /// Snapshot a provider's defaults for a new, unused chat. Existing chats
    /// resolve only their own overrides/native settings, never current defaults.
    pub fn default_configuration(&self, provider: &str) -> AgentConfiguration {
        let defaults = if provider == "claude" {
            &self.preferences.claude_defaults
        } else {
            &self.preferences.codex_defaults
        };
        let model_id = defaults
            .model_id
            .as_deref()
            .unwrap_or(if provider == "claude" {
                "default"
            } else {
                fx_core::DEFAULT_CODEX_MODEL_ID
            });
        let model = crate::provider_catalog::known_model(provider, model_id);
        AgentConfiguration {
            provider_id: Some(provider.into()),
            model_id: defaults.model_id.clone(),
            effort: defaults
                .effort
                .as_ref()
                .filter(|effort| {
                    model
                        .as_ref()
                        .is_none_or(|model| model.supported_efforts.contains(effort))
                })
                .cloned(),
            agent_access: defaults.agent_access,
            context_window_size: model
                .map(|model| model.context_window)
                .filter(|size| *size > 0),
            ..Default::default()
        }
    }

    /// Naming runs independently of the main turn without making the composer busy.
    pub fn has_title_jobs(&self) -> bool {
        !self.title_jobs.is_empty()
    }

    /// Discard results from requests started with previous naming preferences.
    pub(crate) fn cancel_title_jobs(&mut self) {
        self.title_jobs.clear();
    }

    /// Keep the root's poller running without a provider, as a pending naming
    /// request would. Returns the generation that finishes it.
    #[cfg(test)]
    pub(crate) fn hold_title_job(&mut self, agent_id: Uuid) -> Uuid {
        let generation = Uuid::new_v4();
        self.title_jobs.insert(
            agent_id,
            crate::auto_title::TitleJob {
                generation,
                provider: "codex".into(),
                model: "test-model".into(),
            },
        );
        generation
    }

    fn auto_title_record(&self, agent: Uuid) -> Option<&fx_store::PersistedProject> {
        self.snapshot
            .projects
            .iter()
            .find(|record| record.auto_title_pending.contains(&agent))
    }

    fn first_prompt(&self, agent: Uuid) -> Option<String> {
        let conversation = self.conversations.get(&agent)?.lock().ok()?;
        let text = conversation
            .messages()
            .find(|message| message.role == fx_core::MessageRole::User)?
            .text_content();
        Some(
            fx_core::policy::transcript_presentation::user_prompt(&text)
                .text
                .to_owned(),
        )
    }

    fn set_auto_title(&mut self, agent_id: Uuid, title: &str) -> bool {
        if !self.snapshot_writable || self.thread(agent_id).is_none() {
            return false;
        }
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if let Some(agent) = record.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.title = title.to_owned();
                local_override(record, agent_id).title = Some(title.to_owned());
                for binding in &mut record.native_bindings {
                    if binding.agent_id == agent_id {
                        binding.title = title.to_owned();
                    }
                }
                break;
            }
        }
        self.commit_thread_change(next, "save the generated chat name")
    }

    fn show_prompt_title(&mut self, agent: Uuid) {
        if !self.preferences.auto_titles.enabled || self.auto_title_record(agent).is_none() {
            return;
        }
        if let Some(user) = self.first_prompt(agent)
            && let Some(title) = crate::auto_title::fallback_title(&user)
        {
            self.set_auto_title(agent, &title);
        }
    }

    /// Consume eligibility durably before calling a model. Failure, cancellation,
    /// restart and later turns never repeat this automatic paid request.
    fn prepare_auto_title(&mut self, agent_id: Uuid) -> Option<fx_agent::title::TitleRequest> {
        self.auto_title_record(agent_id)?;
        // Only the first accepted prompt can name a chat. Opting out or sending
        // an image-only prompt consumes eligibility without a model request.
        let request = if !self.preferences.auto_titles.enabled || self.is_thread_archived(agent_id)
        {
            None
        } else {
            self.thread(agent_id)?;
            let user = self.first_prompt(agent_id)?;
            let preferences = &self.preferences.auto_titles;
            (!user.trim().is_empty()
                && matches!(preferences.provider_id.as_str(), "codex" | "claude")
                && !preferences.model_id.trim().is_empty())
            .then(|| fx_agent::title::TitleRequest {
                provider: preferences.provider_id.clone(),
                model: preferences.model_id.clone(),
                user,
            })
        };
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            record.auto_title_pending.retain(|id| *id != agent_id);
        }
        if !self.snapshot_writable
            || !self.commit_thread_change(next, "remember the chat naming attempt")
        {
            return None;
        }
        request
    }

    fn start_auto_title(&mut self, agent_id: Uuid) {
        let Some(request) = self.prepare_auto_title(agent_id) else {
            return;
        };
        let generation = Uuid::new_v4();
        self.title_jobs.insert(
            agent_id,
            crate::auto_title::TitleJob {
                generation,
                provider: request.provider.clone(),
                model: request.model.clone(),
            },
        );
        #[cfg(not(test))]
        if let Err(error) =
            crate::auto_title::spawn(request, generation, self.update_sender_for(agent_id))
        {
            self.title_jobs.remove(&agent_id);
            tracing::warn!(%error, "could not start chat naming");
        }
        // Unit/view tests never issue billable provider requests. The runner is
        // exercised separately against fake child processes.
        #[cfg(test)]
        self.title_jobs.remove(&agent_id);
    }

    fn finish_auto_title(
        &mut self,
        agent_id: Uuid,
        generation: Uuid,
        result: Result<String, String>,
    ) {
        if !self
            .title_jobs
            .get(&agent_id)
            .is_some_and(|job| job.generation == generation)
        {
            return;
        }
        let job = self
            .title_jobs
            .remove(&agent_id)
            .expect("matched title job");
        if self.thread(agent_id).is_none() {
            return;
        }
        if !self.preferences.auto_titles.enabled
            || self.is_thread_archived(agent_id)
            || self.preferences.auto_titles.provider_id != job.provider
            || self.preferences.auto_titles.model_id != job.model
        {
            return;
        }
        match result {
            Ok(title) => {
                if let Some(title) = fx_agent::title::clean_title(&title) {
                    self.set_auto_title(agent_id, &title);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "automatic chat naming failed; keeping the existing name")
            }
        }
    }

    /// Whether a project's threads are showing.
    pub fn is_expanded(&self, id: Uuid) -> bool {
        self.snapshot
            .projects
            .iter()
            .find(|record| record.project.as_ref().is_some_and(|p| p.id == id))
            .is_some_and(|record| record.is_expanded)
    }

    pub fn active_project(&self) -> Option<&Project> {
        let id = self.snapshot.active_project_id?;
        self.project(id)
    }

    pub fn project(&self, id: Uuid) -> Option<&Project> {
        self.snapshot
            .projects
            .iter()
            .filter_map(|p| p.project.as_ref())
            .find(|p| p.id == id)
    }

    /// Threads of the active project, in sidebar order.
    pub fn active_agents(&self) -> Vec<Agent> {
        let Some(project) = self.active_project() else {
            return Vec::new();
        };
        let record = match self.snapshot.projects.iter().find(|p| {
            p.project
                .as_ref()
                .is_some_and(|inner| inner.id == project.id)
        }) {
            Some(record) => record,
            None => return Vec::new(),
        };
        let by_id: HashMap<Uuid, &Agent> = record.agents.iter().map(|a| (a.id, a)).collect();
        project
            .agent_order
            .iter()
            .filter(|id| {
                !record.archived_agent_ids.contains(id)
                    && !record.missing_native_agent_ids.contains(id)
            })
            .filter_map(|id| by_id.get(id).map(|a| (*a).clone()))
            .collect()
    }

    pub fn active_agent(&self) -> Option<Agent> {
        let id = self.snapshot.active_agent_id?;
        self.snapshot
            .projects
            .iter()
            .find(|record| {
                record
                    .project
                    .as_ref()
                    .is_some_and(|project| Some(project.id) == self.snapshot.active_project_id)
            })
            .and_then(|record| record.agents.iter().find(|agent| agent.id == id))
            .cloned()
    }

    /// What the centre column titles itself.
    pub fn active_thread_title(&self) -> String {
        match self.active_agent() {
            Some(agent) if !agent.title.is_empty() => agent.title,
            Some(_) => "Untitled thread".to_string(),
            None => "FlowX".to_string(),
        }
    }

    pub fn select_project(&mut self, id: Uuid) {
        let previous = self.snapshot.active_agent_id;
        let Some(record) = self.snapshot.projects.iter().find(|record| {
            record
                .project
                .as_ref()
                .is_some_and(|project| project.id == id)
        }) else {
            return;
        };
        self.snapshot.active_project_id = Some(id);
        self.snapshot.active_agent_id = visible_agent(record, record.last_selected_agent_id);
        self.mark_seen([previous, self.snapshot.active_agent_id]);
        self.request_native_refresh(id);
    }

    /// Whether a thread finished a turn the reader has not opened since.
    pub fn has_unseen_completion(&self, agent_id: Uuid) -> bool {
        self.snapshot
            .projects
            .iter()
            .any(|record| record.unseen_completion_ids.contains(&agent_id))
    }

    /// Returns whether anything changed, so callers save only when it did.
    fn set_unseen_completion(&mut self, agent_id: Uuid, unseen: bool) -> bool {
        let Some(record) = self
            .snapshot
            .projects
            .iter_mut()
            .find(|record| record.agents.iter().any(|agent| agent.id == agent_id))
        else {
            return false;
        };
        if record.unseen_completion_ids.contains(&agent_id) == unseen {
            return false;
        }
        if unseen {
            record.unseen_completion_ids.push(agent_id);
        } else {
            record.unseen_completion_ids.retain(|id| *id != agent_id);
        }
        true
    }

    /// Clear the unseen mark from threads that are, or just were, on screen.
    ///
    /// The thread being left counts as well as the one being opened. Archiving
    /// or deleting the selected thread moves the selection without going
    /// through here, and that thread has been on screen ever since.
    fn mark_seen(&mut self, agents: impl IntoIterator<Item = Option<Uuid>>) {
        let mut changed = false;
        for agent in agents.into_iter().flatten() {
            changed |= self.set_unseen_completion(agent, false);
        }
        if changed {
            self.save();
        }
    }

    pub fn select_agent(&mut self, id: Uuid) {
        let previous = self.snapshot.active_agent_id;
        let Some(record) = self
            .snapshot
            .projects
            .iter_mut()
            .find(|record| record.agents.iter().any(|agent| agent.id == id))
        else {
            return;
        };
        let Some(project) = &record.project else {
            return;
        };
        self.snapshot.active_project_id = Some(project.id);
        self.snapshot.active_agent_id = Some(id);
        record.last_selected_agent_id = Some(id);
        self.native_sync.history_due.remove(&id);
        self.mark_seen([previous, Some(id)]);
    }

    /// Show a thread from anywhere — ⌘1…⌘9 or the palette — the way clicking
    /// it in the sidebar does: its project selected and expanded, and its
    /// transcript read on first selection. Returns whether the thread exists.
    pub fn open_thread(&mut self, agent_id: Uuid) -> bool {
        let Some(project_id) = self.project_for_agent(agent_id) else {
            return false;
        };
        if !self.is_expanded(project_id) {
            self.toggle_project_expansion(project_id);
        }
        if self.snapshot.active_project_id != Some(project_id) {
            self.select_project(project_id);
        }
        self.select_agent(agent_id);
        self.conversation(project_id, agent_id);
        true
    }

    /// Clear a thread's conversation and start over with a new provider session.
    ///
    /// Ported from the Swift app's `resetConversation`. A thread the provider
    /// already knows cannot be emptied from here — its history would come back
    /// with the next sync — so it is kept as it is, and a fresh thread with the
    /// same effective configuration takes its place in the selection. So does a
    /// thread whose saved transcript could not be read: emptying it would make
    /// its files writable again, and they are kept for recovery. Any other
    /// thread is emptied in place: transcript, session, usage, and draft.
    /// Refused while the thread is running, like every other change to a
    /// thread. Returns the thread selected afterwards.
    pub fn reset_conversation(&mut self, agent_id: Uuid, cx: &mut App) -> Option<Uuid> {
        if self.is_thread_busy(agent_id) {
            self.last_error = Some("Stop this thread's running turn before resetting it.".into());
            return None;
        }
        let project_id = self.project_for_agent(agent_id)?;
        let source = self.thread(agent_id)?.clone();
        self.pending_interactions.remove(&agent_id);
        self.retry_drafts.remove(&agent_id);
        let provider_knows_it = self
            .snapshot
            .projects
            .iter()
            .flat_map(|record| &record.native_bindings)
            .any(|binding| binding.agent_id == agent_id);
        if provider_knows_it || self.protected_conversations.contains(&agent_id) {
            let configuration =
                crate::configuration::with_native_defaults(source, self.native_defaults(agent_id))
                    .configuration;
            self.select_agent(agent_id);
            let fresh = self.create_thread()?;
            if let Some(agent) = self
                .snapshot
                .projects
                .iter_mut()
                .flat_map(|record| record.agents.iter_mut())
                .find(|agent| agent.id == fresh)
            {
                agent.configuration = configuration;
            }
            self.save();
            return Some(fresh);
        }
        let conversation = self.conversation(project_id, agent_id);
        *conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Conversation::new(agent_id);
        self.restored_costs.remove(&agent_id);
        // Forget what was last written, so the emptied transcript is saved —
        // which, having nothing worth keeping, deletes the file.
        self.saved_conversations.remove(&agent_id);
        self.clear_draft(agent_id, cx);
        self.save_conversations();
        self.save();
        Some(agent_id)
    }

    pub fn thread(&self, agent_id: Uuid) -> Option<&Agent> {
        self.snapshot
            .projects
            .iter()
            .flat_map(|record| &record.agents)
            .find(|agent| agent.id == agent_id)
    }

    pub fn is_thread_archived(&self, agent_id: Uuid) -> bool {
        self.snapshot
            .projects
            .iter()
            .any(|record| record.archived_agent_ids.contains(&agent_id))
    }

    pub fn is_thread_busy(&self, agent_id: Uuid) -> bool {
        self.runners.contains_key(&agent_id)
            || self
                .conversations
                .get(&agent_id)
                .is_some_and(|conversation| {
                    conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_busy()
                })
    }

    fn thread_can_change(&mut self, agent_id: Uuid) -> bool {
        if !self.snapshot_writable {
            self.last_error = Some(
                "The saved workspace could not be read. Restore its files before changing threads."
                    .into(),
            );
            return false;
        }
        if self.thread(agent_id).is_none() {
            self.last_error = Some("This thread is no longer available.".into());
            return false;
        }
        if self.is_thread_busy(agent_id) {
            self.last_error = Some("Stop this thread's running turn before changing it.".into());
            return false;
        }
        true
    }

    /// Commit metadata before changing the live view. If the backup write fails
    /// after the primary landed, try restoring the previous snapshot as well.
    fn commit_thread_change(&mut self, mut next: PersistedApp, action: &str) -> bool {
        next.sidebar_visible = self.sidebar_visible;
        next.normalize();
        if serde_json::to_vec(&next)
            .is_ok_and(|bytes| bytes.len() as u64 > fx_store::format::MAX_PROJECTS_BYTES)
        {
            self.last_error = Some(format!(
                "Could not {action}: native thread metadata exceeds the workspace file size limit. The previous workspace has been preserved."
            ));
            return false;
        }
        if let Err(error) = self.store.save_app(&next) {
            let rollback = self.store.save_app(&self.snapshot);
            self.last_error = Some(match rollback {
                Ok(()) => format!("Could not {action}: {error}"),
                Err(rollback) => format!(
                    "Could not {action}: {error}. Could not confirm the previous saved state: {rollback}"
                ),
            });
            return false;
        }
        self.snapshot = next;
        true
    }

    pub fn rename_thread(&mut self, agent_id: Uuid, title: &str) -> bool {
        if !self.thread_can_change(agent_id) {
            return false;
        }
        let title = title.trim();
        if title.is_empty() {
            self.last_error = Some("Enter a name for this thread.".into());
            return false;
        }
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if let Some(agent) = record.agents.iter_mut().find(|agent| agent.id == agent_id) {
                agent.title = title.to_string();
                record.auto_title_pending.retain(|id| *id != agent_id);
                local_override(record, agent_id).title = Some(title.to_string());
                for binding in &mut record.native_bindings {
                    if binding.agent_id == agent_id {
                        binding.title = title.to_string();
                    }
                }
                break;
            }
        }
        let committed = self.commit_thread_change(next, "rename this thread");
        if committed {
            self.title_jobs.remove(&agent_id);
        }
        committed
    }

    pub fn set_thread_archived(&mut self, agent_id: Uuid, archived: bool) -> bool {
        if !self.thread_can_change(agent_id) {
            return false;
        }
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if !record.agents.iter().any(|agent| agent.id == agent_id) {
                continue;
            }
            local_override(record, agent_id).archived = Some(archived);
            if archived {
                record.archived_agent_ids.push(agent_id);
                if record.last_selected_agent_id == Some(agent_id) {
                    record.last_selected_agent_id = visible_agent(record, None);
                }
                if next.active_agent_id == Some(agent_id) {
                    next.active_agent_id = visible_agent(record, record.last_selected_agent_id);
                }
            } else {
                record.archived_agent_ids.retain(|id| *id != agent_id);
                record.missing_native_agent_ids.retain(|id| *id != agent_id);
            }
            break;
        }
        if !self.commit_thread_change(
            next,
            if archived {
                "archive this thread"
            } else {
                "restore this thread"
            },
        ) {
            return false;
        }
        if archived {
            self.pending_interactions.remove(&agent_id);
            self.title_jobs.remove(&agent_id);
        }
        true
    }

    /// Remove only FlowX's local thread and cached history. Provider records and
    /// files in the project directory are outside this operation.
    pub fn delete_thread(&mut self, agent_id: Uuid) -> bool {
        if !self.thread_can_change(agent_id) {
            return false;
        }
        let Some(project_id) = self.project_for_agent(agent_id) else {
            return false;
        };
        // Older Rust-created threads may have a session only in their cache.
        self.bind_local_session(agent_id);
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if !record.agents.iter().any(|agent| agent.id == agent_id) {
                continue;
            }
            record.agents.retain(|agent| agent.id != agent_id);
            for binding in record
                .native_bindings
                .iter()
                .filter(|binding| binding.agent_id == agent_id)
            {
                let identity = fx_store::format::PersistedNativeIdentity {
                    provider_id: binding.provider_id.clone(),
                    session_id: binding.session_id.clone(),
                };
                if !record.suppressed_native_threads.contains(&identity) {
                    record.suppressed_native_threads.push(identity);
                }
            }
            record
                .thread_overrides
                .retain(|entry| entry.agent_id != agent_id);
            record.missing_native_agent_ids.retain(|id| *id != agent_id);
            record.archived_agent_ids.retain(|id| *id != agent_id);
            record.unseen_completion_ids.retain(|id| *id != agent_id);
            if let Some(project) = &mut record.project {
                project.agent_order.retain(|id| *id != agent_id);
            }
            record
                .workspaces
                .retain(|workspace| workspace.agent_id != agent_id);
            record
                .native_bindings
                .retain(|binding| binding.agent_id != agent_id);
            record
                .native_presentation_ids
                .retain(|presentation| presentation.agent_id != agent_id);
            if record.last_selected_agent_id == Some(agent_id) {
                record.last_selected_agent_id = visible_agent(record, None);
            }
            if next.active_agent_id == Some(agent_id) {
                next.active_agent_id = visible_agent(record, record.last_selected_agent_id);
            }
            break;
        }
        if !self.commit_thread_change(next, "delete this thread") {
            return false;
        }
        self.deleted_agents.insert(agent_id);
        self.title_jobs.remove(&agent_id);
        self.conversations.remove(&agent_id);
        self.drafts.remove(&agent_id);
        self.runners.remove(&agent_id);
        self.protected_conversations.remove(&agent_id);
        self.restored_drafts.remove(&agent_id);
        self.retry_drafts.remove(&agent_id);
        self.saved_conversations.remove(&agent_id);
        self.restored_costs.remove(&agent_id);
        self.pending_interactions.remove(&agent_id);
        self.native_sync.invalidate(project_id);
        self.native_sync.history_due.remove(&agent_id);
        self.native_sync.history_warnings.remove(&agent_id);
        self.native_sync.completed_turns.remove(&agent_id);
        self.native_sync.loaded_revisions.remove(&agent_id);
        // A queued save landing after this would recreate the deleted files.
        self.writer.forget(agent_id);
        crate::transcript::forget_thread(agent_id);
        if let Err(error) = self.store.delete_thread_files(project_id, agent_id) {
            self.last_error = Some(format!(
                "The thread was removed, but its saved files could not all be deleted: {error}"
            ));
        }
        true
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.snapshot.sidebar_visible = self.sidebar_visible;
    }

    /// Save the layout once a panel toggle has finished moving.
    ///
    /// A save flushes the workspace file and its backup to disk, which takes
    /// long enough on the UI thread to cost the slide its first frames. A
    /// second toggle before then replaces the pending save.
    pub fn save_after_panel_moves(&mut self, cx: &mut Context<Self>) {
        self.panel_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(fx_design::theme::FxAnimation::PANEL + Duration::from_millis(50))
                .await;
            let _ = this.update(cx, |state, _| state.save());
        }));
    }

    pub fn sidebar_width(&self) -> f32 {
        Layout::clamp_sidebar_width(self.snapshot.sidebar_width)
    }

    /// Resize the project list, retaining its width while the sidebar is hidden.
    pub fn set_sidebar_width(&mut self, width: f32) {
        self.snapshot.sidebar_width = Layout::clamp_sidebar_width(width);
    }

    pub fn toggle_inspector(&mut self) {
        self.snapshot.right_panel_visible = !self.snapshot.right_panel_visible;
    }

    pub fn right_panel_width(&self) -> f32 {
        self.snapshot
            .right_panel_width
            .clamp(Layout::RIGHT_PANEL_MIN_WIDTH, Layout::RIGHT_PANEL_MAX_WIDTH)
    }

    /// Move the inspector, keeping it inside its legal range.
    pub fn set_right_panel_width(&mut self, width: f32) {
        self.snapshot.right_panel_width =
            width.clamp(Layout::RIGHT_PANEL_MIN_WIDTH, Layout::RIGHT_PANEL_MAX_WIDTH);
    }

    pub fn toggle_project_expansion(&mut self, id: Uuid) {
        if let Some(record) = self
            .snapshot
            .projects
            .iter_mut()
            .find(|p| p.project.as_ref().is_some_and(|inner| inner.id == id))
        {
            record.is_expanded = !record.is_expanded;
        }
    }

    /// The transcript for a thread, reading it from disk on first use.
    ///
    /// Blocks on that read, so it is for actions that cannot proceed without
    /// the history. A view uses [`Self::request_conversation`] instead.
    pub fn conversation(&mut self, project_id: Uuid, agent_id: Uuid) -> Arc<GuardedConversation> {
        if let Some(existing) = self.conversations.get(&agent_id) {
            return existing.clone();
        }
        let loaded = read_conversation(&self.store, project_id, agent_id);
        self.adopt_conversation(project_id, agent_id, loaded)
    }

    /// The transcript for a thread, or `None` while it is read in the background.
    ///
    /// The first call for an unloaded thread starts the read; when it lands,
    /// the state notifies, so whatever asked repaints with the history.
    pub fn request_conversation(
        &mut self,
        project_id: Uuid,
        agent_id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<Arc<GuardedConversation>> {
        if let Some(found) = self.loaded_or_unsaved(project_id, agent_id) {
            return Some(found);
        }
        if self.loading.insert(agent_id) {
            let store = self.store.clone();
            let read =
                cx.background_spawn(async move { read_conversation(&store, project_id, agent_id) });
            cx.spawn(async move |this, cx| {
                let loaded = read.await;
                let _ = this.update(cx, |state, cx| {
                    state.loading.remove(&agent_id);
                    // A blocking load may have won the race, and a thread can be
                    // deleted or moved while its file is read. Neither may be
                    // replaced by a late result.
                    if state.conversations.contains_key(&agent_id)
                        || state.deleted_agents.contains(&agent_id)
                        || state.project_for_agent(agent_id) != Some(project_id)
                    {
                        return;
                    }
                    state.adopt_conversation(project_id, agent_id, loaded);
                    state.restore_loaded_draft(agent_id, cx);
                    cx.notify();
                });
            })
            .detach();
        }
        None
    }

    /// A thread's transcript when it can be had without reading a file: it is
    /// loaded already, or nothing was ever saved for it. For callers that only
    /// need what the transcript says so far, and must not start a read.
    pub(crate) fn loaded_or_unsaved(
        &mut self,
        project_id: Uuid,
        agent_id: Uuid,
    ) -> Option<Arc<GuardedConversation>> {
        if let Some(existing) = self.conversations.get(&agent_id) {
            return Some(existing.clone());
        }
        if self.loading.contains(&agent_id) || self.store.has_conversation(project_id, agent_id) {
            return None;
        }
        Some(self.adopt_conversation(project_id, agent_id, LoadedConversation::empty(agent_id)))
    }

    /// Whether a thread's transcript is still being read.
    pub fn is_loading_conversation(&self, agent_id: Uuid) -> bool {
        self.loading.contains(&agent_id)
    }

    /// Install a read transcript as the live one.
    fn adopt_conversation(
        &mut self,
        project_id: Uuid,
        agent_id: Uuid,
        loaded: LoadedConversation,
    ) -> Arc<GuardedConversation> {
        let LoadedConversation {
            mut conversation,
            restored,
            failure,
            goal,
            attachments,
        } = loaded;
        if let Some(goal) = goal {
            self.composer.goals.insert(agent_id, goal);
        }
        if !attachments.is_empty() {
            self.composer
                .attachments
                .entry(agent_id)
                .or_insert(attachments);
        }
        if let Some(reason) = failure {
            self.protected_conversations.insert(agent_id);
            self.last_error = Some(format!(
                "Could not restore this conversation: {reason}. Its saved files have been preserved."
            ));
        }
        if conversation.session_id().is_none()
            && let Some(binding) = self
                .snapshot
                .projects
                .iter()
                .filter(|record| {
                    record
                        .project
                        .as_ref()
                        .is_some_and(|project| project.id == project_id)
                })
                .flat_map(|record| &record.native_bindings)
                .find(|binding| binding.agent_id == agent_id)
        {
            conversation.set_session_id(binding.session_id.clone());
        }
        if let Some((cost, draft)) = restored {
            self.restored_costs.insert(agent_id, cost);
            // The file holds exactly this, so opening a thread is not a reason
            // to write it back.
            self.saved_conversations
                .insert(agent_id, (conversation.revision(), draft.clone()));
            self.restored_drafts.insert(agent_id, draft);
        }
        let conversation = Arc::new(Mutex::new(conversation));
        self.conversations.insert(agent_id, conversation.clone());
        conversation
    }

    /// Put a draft read in the background into a composer made while it was
    /// read, unless the user has typed there since: that typing is newer.
    fn restore_loaded_draft(&mut self, agent_id: Uuid, cx: &mut App) {
        let Some(field) = self.drafts.get(&agent_id).cloned() else {
            return;
        };
        let typed = field.read(cx).value().to_string();
        if !typed.is_empty() {
            self.restored_drafts.insert(agent_id, typed);
        } else if let Some(draft) = self
            .restored_drafts
            .get(&agent_id)
            .filter(|draft| !draft.is_empty())
            .cloned()
        {
            field.update(cx, |field, cx| field.set_value(draft, cx));
        }
    }

    /// The active thread's conversation, or `None` while it is still loading.
    ///
    /// `None` also when no thread is selected. Separate from
    /// [`Self::conversation`] because a view asks this every frame and must not
    /// read a transcript inside a render: the first ask starts a background
    /// read, and the transcript repaints when it lands.
    pub fn active_conversation(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Arc<GuardedConversation>> {
        let project_id = self.snapshot.active_project_id?;
        let agent_id = self.snapshot.active_agent_id?;
        self.request_conversation(project_id, agent_id, cx)
    }

    /// The draft field for a thread, creating it on first use.
    ///
    /// A saved draft arrives with its transcript. When that is still being
    /// read, the field starts empty and is filled once the read lands.
    pub fn draft(&mut self, agent_id: Uuid, cx: &mut Context<Self>) -> Entity<TextField> {
        if let Some(existing) = self.drafts.get(&agent_id) {
            return existing.clone();
        }
        if let Some(project_id) = self.project_for_agent(agent_id) {
            self.request_conversation(project_id, agent_id, cx);
        }
        let initial = self
            .restored_drafts
            .get(&agent_id)
            .cloned()
            .unwrap_or_default();
        let field = cx.new(|cx| {
            let mut field = TextField::new(cx)
                .bare()
                .placeholder("Ask anything, or describe a task…")
                .multiline(8.0)
                .max_len(32_000);
            field.set_value(initial, cx);
            field
        });
        self.drafts.insert(agent_id, field.clone());
        field
    }

    /// Add a project at an already-validated, resolved path, and select it.
    ///
    /// The path arrives from `project_picker::validate_project_path`, so it has
    /// been canonicalized. Re-checking here would still let the user add a
    /// symlink and have it retargeted underneath them later.
    ///
    /// A folder that is already open is focused rather than added again. Two
    /// rows for one root would race each other for the same provider threads,
    /// and the loader keeps only the first of them on the next launch, taking
    /// the second row's threads with it.
    pub fn add_project(&mut self, root_path: &str) -> Uuid {
        if let Some(existing) = self.project_with_root(root_path) {
            if let Some(record) = self
                .snapshot
                .projects
                .iter_mut()
                .find(|record| record.project.as_ref().is_some_and(|p| p.id == existing))
            {
                record.is_expanded = true;
            }
            self.select_project(existing);
            self.save();
            return existing;
        }
        let project = Project::new(root_path);
        self.snapshot.projects.push(fx_store::PersistedProject {
            project: Some(project.clone()),
            is_expanded: true,
            ..Default::default()
        });
        self.snapshot.active_project_id = Some(project.id);
        // A new project has no threads, so a carried-over agent selection would
        // name an agent that does not exist.
        self.snapshot.active_agent_id = None;
        self.request_native_refresh(project.id);
        self.save();
        project.id
    }

    /// The open project whose folder is `root_path`.
    ///
    /// Compares resolved paths, so a symlink, a trailing slash, or `~` names the
    /// same project as the folder it points at. Projects saved before paths were
    /// canonicalised are resolved here too, not only the one being added.
    pub fn project_with_root(&self, root_path: &str) -> Option<Uuid> {
        let wanted = Project::new(root_path).canonical_root_path();
        self.snapshot
            .projects
            .iter()
            .filter_map(|record| record.project.as_ref())
            .find(|project| project.canonical_root_path() == wanted)
            .map(|project| project.id)
    }

    /// Every open project's resolved folder, for the picker to recognise.
    pub fn open_project_roots(&self) -> Vec<std::path::PathBuf> {
        self.snapshot
            .projects
            .iter()
            .filter_map(|record| record.project.as_ref())
            .map(Project::canonical_root_path)
            .collect()
    }

    /// Remove only the sidebar entry. Saved transcripts and project files remain.
    ///
    /// Callers capture drafts first (`save_all`), because the transcripts are
    /// written once more here and then released from memory.
    pub fn remove_project(&mut self, project_id: Uuid) {
        let agents: Vec<Uuid> = self
            .snapshot
            .projects
            .iter()
            .filter(|record| record.project.as_ref().is_some_and(|p| p.id == project_id))
            .flat_map(|record| record.agents.iter().map(|agent| agent.id))
            .collect();
        // The same test the project menu uses to disable the row, so a turn
        // that is still starting or waiting on an answer keeps its project.
        if agents.iter().any(|id| self.is_thread_busy(*id)) {
            self.last_error = Some("Stop this project's running turns before removing it.".into());
            return;
        }
        self.save_conversations();
        self.native_sync.invalidate(project_id);
        self.snapshot
            .projects
            .retain(|record| !record.project.as_ref().is_some_and(|p| p.id == project_id));
        for agent_id in agents {
            self.release_thread(agent_id);
        }
        if self.snapshot.active_project_id == Some(project_id) {
            self.snapshot.active_project_id = None;
            self.snapshot.active_agent_id = None;
            if let Some(id) = self
                .snapshot
                .projects
                .iter()
                .find_map(|record| record.project.as_ref().map(|p| p.id))
            {
                self.select_project(id);
            }
        }
    }

    /// Drop everything held in memory for a thread that has left the snapshot
    /// with its project. The in-memory half of `delete_thread`'s cleanup; the
    /// files on disk are kept, and nothing is recorded as deleted, because the
    /// thread was not.
    ///
    /// Only reached for idle threads: `remove_project` refuses while any of its
    /// threads is busy, so no running turn loses its conversation or runner.
    fn release_thread(&mut self, agent_id: Uuid) {
        self.title_jobs.remove(&agent_id);
        self.conversations.remove(&agent_id);
        self.drafts.remove(&agent_id);
        self.protected_conversations.remove(&agent_id);
        self.restored_drafts.remove(&agent_id);
        self.retry_drafts.remove(&agent_id);
        self.saved_conversations.remove(&agent_id);
        self.restored_costs.remove(&agent_id);
        self.pending_interactions.remove(&agent_id);
        self.native_sync.history_due.remove(&agent_id);
        self.native_sync.history_warnings.remove(&agent_id);
        self.native_sync.completed_turns.remove(&agent_id);
        self.native_sync.loaded_revisions.remove(&agent_id);
    }

    fn project_for_agent(&self, agent_id: Uuid) -> Option<Uuid> {
        self.snapshot.projects.iter().find_map(|record| {
            record
                .agents
                .iter()
                .any(|agent| agent.id == agent_id)
                .then(|| record.project.as_ref().map(|project| project.id))
                .flatten()
        })
    }

    /// Move the prompt into the transcript and recovery record before starting
    /// a worker. The composer is immediately available for the next draft.
    ///
    /// The record's save is queued here, ahead of anything the turn writes; it
    /// lands on the writer's thread rather than holding up the send.
    pub fn prepare_prompt(&mut self, agent_id: Uuid, text: &str, cx: &mut App) -> bool {
        self.prepare_prompt_with(agent_id, text, &[], cx)
    }

    /// [`Self::prepare_prompt`], with the images that go with it.
    ///
    /// The images ride in the user's row, so a retry and a restart recover
    /// them from the transcript along with the unsent prompt's text.
    pub fn prepare_prompt_with(
        &mut self,
        agent_id: Uuid,
        text: &str,
        attachments: &[fx_core::Attachment],
        cx: &mut App,
    ) -> bool {
        if self.runners.contains_key(&agent_id) || self.protected_conversations.contains(&agent_id)
        {
            return false;
        }
        if self.is_thread_archived(agent_id) && !self.set_thread_archived(agent_id, false) {
            return false;
        }
        let Some(project_id) = self.project_for_agent(agent_id) else {
            return false;
        };
        let conversation = self.conversation(project_id, agent_id);
        if self.protected_conversations.contains(&agent_id) {
            return false;
        }
        {
            let mut conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if conversation.is_busy() {
                return false;
            }
            // A prompt rejected before acknowledgment is already displayed as a
            // user row. Retrying it must not add a second copy of the same row.
            let retrying_same_row = conversation
                .unsent()
                .is_some_and(|prompt| prompt.text == text)
                && conversation.messages().last().is_some_and(|message| {
                    message.role == fx_core::MessageRole::User
                        && message.text_content() == text
                        && same_images(&crate::attachments::attachments_in(message), attachments)
                });
            if !retrying_same_row {
                conversation.append(crate::attachments::user_message(text, attachments));
            }
            let session_id = conversation.session_id().map(str::to_string);
            conversation.hold_unsent(fx_agent::conversation::UnsentPrompt {
                text: text.to_string(),
                attachment_count: attachments.len(),
                session_id,
            });
        }
        self.retry_drafts.remove(&agent_id);
        self.composer.retry_attachments.remove(&agent_id);
        let sent: Vec<Uuid> = attachments.iter().map(|attachment| attachment.id).collect();
        self.release_sent_attachments(agent_id, &sent);
        // The keyboard callback is deferred. Only clear the submitted text,
        // preserving any replacement draft written before that callback runs.
        if let Some(field) = self.drafts.get(&agent_id).cloned() {
            if field.read(cx).value().trim() == text.trim() {
                self.clear_draft(agent_id, cx);
            }
        } else if self
            .restored_drafts
            .get(&agent_id)
            .is_some_and(|draft| draft.trim() == text.trim())
        {
            self.restored_drafts.remove(&agent_id);
        }
        self.capture_drafts(cx);
        self.native_sync.invalidate(project_id);
        self.save();
        self.save_conversations();
        true
    }

    /// Where a turn reports what it did.
    ///
    /// Kept as a sender the app can clone out, so the runner's callback can be
    /// `Send` without holding anything else.
    pub fn update_sender(&self) -> std::sync::mpsc::Sender<crate::session::SessionUpdate> {
        self.update_tx.clone()
    }

    /// Bind updates to their originating thread, independent of the current selection.
    pub fn update_sender_for(
        &self,
        agent_id: Uuid,
    ) -> impl Fn(crate::session::SessionUpdate) + Send + 'static {
        let sender = self.targeted_tx.clone();
        move |update| {
            if matches!(update, crate::session::SessionUpdate::Changed) {
                // A pending redraw already exposes the whole live conversation.
                // Coalesce redundant wakeups rather than accumulating every delta.
                let _ = sender.try_send((agent_id, update));
            } else {
                // Lifecycle and interaction updates must never be discarded.
                let _ = sender.send((agent_id, update));
            }
        }
    }

    pub fn register_runner(&mut self, agent_id: Uuid, runner: crate::session::TurnRunner) {
        self.runners.insert(agent_id, runner);
    }

    /// Apply anything the running turn has reported, and how queued saves went.
    ///
    /// Returns whether anything was applied, which is what tells the root's
    /// poller to repaint: a turn finishing must repaint without anything asking
    /// it to, and a tick with nothing to report must not.
    pub fn drain_updates(&mut self) -> bool {
        let mut applied = self.apply_save_outcomes();
        let updates: Vec<crate::session::SessionUpdate> = self.updates.try_iter().collect();
        for update in updates {
            applied = true;
            self.apply_update(self.snapshot.active_agent_id, update);
        }
        let updates: Vec<_> = self.targeted_updates.try_iter().collect();
        for (agent_id, update) in updates {
            applied = true;
            self.apply_update(Some(agent_id), update);
        }
        applied
    }

    pub fn drain_updates_with_context(&mut self, cx: &mut App) -> bool {
        // Lifecycle events may save immediately. Capture newer typing first so
        // recovery cannot persist an older draft over a follow-up.
        self.capture_drafts(cx);
        let applied = self.drain_updates();
        for (agent_id, prompt) in std::mem::take(&mut self.retry_drafts) {
            let mut restored = false;
            if let Some(field) = self.drafts.get(&agent_id).cloned() {
                // Never overwrite a follow-up drafted while startup was pending.
                // Whitespace is not a follow-up: `prepare_prompt` trims, so a
                // field holding only "\n" would otherwise swallow the retry.
                if field.read(cx).value().trim().is_empty() {
                    field.update(cx, |field, cx| field.set_value(prompt, cx));
                    restored = true;
                }
            } else if self
                .restored_drafts
                .get(&agent_id)
                .is_none_or(|draft| draft.trim().is_empty())
            {
                self.restored_drafts.insert(agent_id, prompt);
                restored = true;
            }
            // The images come back with the words, and only into a composer
            // that has none of its own by now.
            let images = self.composer.retry_attachments.remove(&agent_id);
            if restored
                && let Some(images) = images
                && self.attachments(agent_id).is_empty()
            {
                self.composer.attachments.insert(agent_id, images);
            }
        }
        self.clear_accepted_steers(cx);
        self.capture_drafts(cx);
        applied
    }

    /// The revisions of threads with a running turn. A runner writes into its
    /// conversation from its own thread, so this is how the root tells a quiet
    /// poll from one with something new to draw.
    pub fn running_revisions(&self) -> Vec<(Uuid, u64)> {
        let mut revisions: Vec<_> = self
            .runners
            .keys()
            .filter_map(|id| {
                let conversation = self.conversations.get(id)?;
                let revision = conversation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .revision();
                Some((*id, revision))
            })
            .collect();
        revisions.sort_unstable();
        revisions
    }

    fn apply_update(&mut self, agent_id: Option<Uuid>, update: crate::session::SessionUpdate) {
        use crate::session::SessionUpdate;
        if let SessionUpdate::AutoTitle { generation, result } = update {
            if let Some(agent) = agent_id {
                self.finish_auto_title(agent, generation, result);
            }
            return;
        }
        if agent_id.is_some_and(|id| self.deleted_agents.contains(&id)) {
            return;
        }
        let mut should_save = true;
        match update {
            SessionUpdate::Started { thread_id } => {
                if let Some(conversation) = agent_id.and_then(|id| self.conversations.get(&id)) {
                    conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .set_session_id(thread_id);
                }
                if let Some(agent_id) = agent_id {
                    self.bind_local_session(agent_id);
                    self.mark_native_local_turn(agent_id);
                    self.save();
                }
            }
            SessionUpdate::Accepted => {
                if let Some(agent_id) = agent_id
                    && let Some(conversation) = self.conversations.get(&agent_id)
                {
                    let mut conversation = conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    conversation.clear_unsent();
                }
                if let Some(agent_id) = agent_id {
                    self.show_prompt_title(agent_id);
                    self.start_auto_title(agent_id);
                }
                // Acknowledgement only releases the recovery copy. The composer
                // may already contain the next prompt, even identical text.
            }
            SessionUpdate::Changed => should_save = false,
            SessionUpdate::Interaction { event } => {
                if let Some(agent_id) = agent_id {
                    let queue = self.pending_interactions.entry(agent_id).or_default();
                    if !queue
                        .iter()
                        .any(|existing| interaction_id(existing) == interaction_id(&event))
                    {
                        queue.push_back(event);
                    }
                }
            }
            SessionUpdate::InteractionResolved { id } => {
                if let Some(queue) =
                    agent_id.and_then(|agent_id| self.pending_interactions.get_mut(&agent_id))
                {
                    queue.retain(|event| interaction_id(event) != Some(&id));
                }
            }
            SessionUpdate::Steered { id, result } => {
                should_save = result.is_ok();
                if let Some(agent_id) = agent_id {
                    self.steer_answered(agent_id, id, result);
                }
            }
            SessionUpdate::GoalChanged { goal } => {
                if let Some(agent_id) = agent_id {
                    self.goal_changed(agent_id, goal);
                }
            }
            SessionUpdate::Finished { outcome } => {
                if let Some(agent_id) = agent_id {
                    self.mark_native_local_turn(agent_id);
                    self.runners.remove(&agent_id);
                    self.pending_interactions.remove(&agent_id);
                    self.composer.steers.remove(&agent_id);
                    self.composer.goal_prompts.remove(&agent_id);
                    self.restore_unaccepted_prompt(agent_id);
                    self.queue_may_advance(agent_id);
                    let settled = crate::thread_presentation::settled_state(&outcome);
                    // Marked before the outcome is recorded, so the one save
                    // that records the outcome carries the mark as well.
                    let marked = crate::thread_presentation::completion_needs_attention(
                        &outcome,
                        self.snapshot.active_agent_id == Some(agent_id),
                    ) && self.set_unseen_completion(agent_id, true);
                    if marked
                        && self
                            .thread(agent_id)
                            .is_some_and(|agent| agent.execution_state == settled)
                    {
                        self.save();
                    }
                    self.record_execution_state(agent_id, settled);
                }
                // A turn that worked clears the previous turn's failure. Leaving
                // it up would report an error that no longer describes anything.
                self.last_error = match outcome {
                    fx_agent::codex::TurnOutcome::Failed { reason, .. } => Some(reason),
                    fx_agent::codex::TurnOutcome::Completed { .. }
                    | fx_agent::codex::TurnOutcome::Cancelled { .. } => None,
                };
            }
            SessionUpdate::Failed { reason } => {
                if let Some(agent_id) = agent_id {
                    self.mark_native_local_turn(agent_id);
                    self.runners.remove(&agent_id);
                    self.pending_interactions.remove(&agent_id);
                    self.composer.steers.remove(&agent_id);
                    self.composer.goal_prompts.remove(&agent_id);
                    self.restore_unaccepted_prompt(agent_id);
                    self.queue_may_advance(agent_id);
                    self.record_execution_state(agent_id, fx_core::AgentExecutionState::Failure);
                }
                self.last_error = Some(reason);
            }
            SessionUpdate::Refused { tool, reason } => {
                tracing::info!(tool, reason, "a tool call was refused");
            }
            SessionUpdate::AutoTitle { .. } => unreachable!("handled before conversation updates"),
        }
        if should_save {
            self.save_conversations();
        }
    }

    /// The unanswered question the dialog shows, the active thread's first.
    ///
    /// Approvals are never here: they are answered on their own card in the
    /// transcript, and a dialog over that card would hide what it asks about.
    pub fn next_question(&self) -> Option<(Uuid, fx_agent::provider::UserInputRequest)> {
        fn first(
            queue: &VecDeque<fx_agent::ProviderEvent>,
        ) -> Option<fx_agent::provider::UserInputRequest> {
            queue.iter().find_map(|event| match event {
                fx_agent::ProviderEvent::UserInput(request) => Some(request.clone()),
                _ => None,
            })
        }
        if let Some(id) = self.snapshot.active_agent_id
            && let Some(request) = self.pending_interactions.get(&id).and_then(first)
        {
            return Some((id, request));
        }
        self.pending_interactions
            .iter()
            .find_map(|(&id, queue)| first(queue).map(|request| (id, request)))
    }

    /// Whether a request can still be answered: its turn is running and
    /// nothing has been sent for it yet.
    pub fn awaits_answer(&self, agent_id: Uuid, id: &str) -> bool {
        self.runners.contains_key(&agent_id)
            && self
                .pending_interactions
                .get(&agent_id)
                .is_some_and(|queue| queue.iter().any(|event| interaction_id(event) == Some(id)))
    }

    /// Save how a thread's turn ended. The sidebar reads this for every row,
    /// loaded or not, so a status cannot depend on whether a thread was opened.
    fn record_execution_state(&mut self, agent_id: Uuid, state: fx_core::AgentExecutionState) {
        let Some(agent) = self
            .snapshot
            .projects
            .iter_mut()
            .flat_map(|record| record.agents.iter_mut())
            .find(|agent| agent.id == agent_id)
        else {
            return;
        };
        if agent.execution_state != state {
            agent.execution_state = state;
            self.save();
        }
    }

    fn restore_unaccepted_prompt(&mut self, agent_id: Uuid) {
        if let Some(conversation) = self.conversations.get(&agent_id) {
            let conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(prompt) = conversation.unsent() {
                self.retry_drafts.insert(agent_id, prompt.text.clone());
                if prompt.attachment_count > 0 {
                    self.composer
                        .retry_attachments
                        .insert(agent_id, unsent_attachments(&conversation));
                }
            }
        }
    }

    /// Send the reader's answer to a pending request, once.
    ///
    /// `false` for a request that is not waiting, including one already
    /// answered, so a double click reaches the provider as one reply.
    pub fn respond_to_interaction(
        &mut self,
        agent_id: Uuid,
        response: fx_agent::provider::UserResponse,
    ) -> bool {
        let Some(queue) = self.pending_interactions.get_mut(&agent_id) else {
            return false;
        };
        let Some(position) = queue
            .iter()
            .position(|event| interaction_id(event) == Some(response.id()))
        else {
            return false;
        };
        if !self
            .runners
            .get(&agent_id)
            .is_some_and(|runner| runner.respond(response))
        {
            return false;
        }
        queue.remove(position);
        true
    }

    /// The conversation for a thread, if it has been loaded.
    pub fn conversation_by_agent(&self, agent_id: Uuid) -> Option<Arc<GuardedConversation>> {
        self.conversations.get(&agent_id).cloned()
    }

    // ── Recovering a failed turn ───────────────────────────────────────────
    //
    // Each of these resets what its action resets and returns the prompt to
    // send, if any. Starting the turn is left to the shell, which owns the path
    // that starts one; a second path here would be a second place for a launch
    // to go wrong.

    /// A loaded thread with nothing running, ready to be recovered.
    fn idle_conversation(&self, agent_id: Uuid) -> Option<Arc<GuardedConversation>> {
        if self.is_thread_busy(agent_id) || self.protected_conversations.contains(&agent_id) {
            return None;
        }
        self.conversations.get(&agent_id).cloned()
    }

    /// Take the turn's failure off the conversation, and off the shell's banner
    /// when that banner is reporting the same failure. An unrelated one, such
    /// as a failed save, stays up.
    fn release_turn_error(&mut self, conversation: &mut Conversation) {
        let error = conversation.error().map(str::to_owned);
        conversation.dismiss_error();
        if error.is_some() && self.last_error == error {
            self.last_error = None;
        }
    }

    /// The images of the prompt a retry or restart resends, read from its own
    /// transcript row.
    pub fn unsent_images(&self, agent_id: Uuid) -> Vec<fx_core::Attachment> {
        self.conversations
            .get(&agent_id)
            .map(|conversation| {
                unsent_attachments(
                    &conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                )
            })
            .unwrap_or_default()
    }

    /// The prompt a retry resends: the one the provider never accepted, else
    /// the last one the reader sent.
    pub fn retry_prompt(&mut self, agent_id: Uuid) -> Option<String> {
        let conversation = self.idle_conversation(agent_id)?;
        let mut conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let prompt = conversation
            .unsent()
            .map(|prompt| prompt.text.clone())
            .or_else(|| conversation.last_user_prompt())
            .filter(|prompt| !prompt.trim().is_empty())?;
        self.release_turn_error(&mut conversation);
        Some(prompt)
    }

    /// Ask the session the failed turn ran in to carry on.
    pub fn resume_session(&mut self, agent_id: Uuid) -> Option<String> {
        let conversation = self.idle_conversation(agent_id)?;
        let mut conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if conversation.session_id().is_none_or(str::is_empty) {
            return None;
        }
        self.release_turn_error(&mut conversation);
        Some("continue".to_string())
    }

    /// Open a fresh provider session on the same thread.
    ///
    /// The thread keeps its transcript and its place in the sidebar; only the
    /// provider's side starts over. The old session's binding goes, so the new
    /// session binds when it starts, and its identity is suppressed so a dead
    /// session cannot come back from discovery as a duplicate thread.
    pub fn restart_session(&mut self, agent_id: Uuid) -> Option<String> {
        let conversation = self.idle_conversation(agent_id)?;
        let prompt = {
            let mut conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            conversation.clear_session_id();
            self.release_turn_error(&mut conversation);
            conversation.record_activity("Session restarted", false);
            conversation
                .unsent()
                .map(|prompt| prompt.text.clone())
                .or_else(|| conversation.last_user_prompt())
                .filter(|prompt| !prompt.trim().is_empty())
        };
        for record in &mut self.snapshot.projects {
            let retired: Vec<fx_store::format::PersistedNativeIdentity> = record
                .native_bindings
                .iter()
                .filter(|binding| binding.agent_id == agent_id)
                .map(|binding| fx_store::format::PersistedNativeIdentity {
                    provider_id: binding.provider_id.clone(),
                    session_id: binding.session_id.clone(),
                })
                .collect();
            record
                .native_bindings
                .retain(|binding| binding.agent_id != agent_id);
            for identity in retired {
                if !record.suppressed_native_threads.contains(&identity) {
                    record.suppressed_native_threads.push(identity);
                }
            }
        }
        self.native_sync.history_due.remove(&agent_id);
        self.native_sync.loaded_revisions.remove(&agent_id);
        self.save();
        self.save_conversations();
        prompt
    }

    /// Put a failed turn's banner away.
    pub fn dismiss_turn_error(&mut self, agent_id: Uuid) {
        let Some(conversation) = self.conversations.get(&agent_id).cloned() else {
            return;
        };
        self.release_turn_error(
            &mut conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        self.save_conversations();
    }

    /// The thread and project root a recovery turn runs with.
    pub fn recovery_target(&self, agent_id: Uuid) -> Option<(Agent, String)> {
        let agent = self.thread(agent_id)?.clone();
        let root = self
            .project(self.project_for_agent(agent_id)?)?
            .root_path
            .clone();
        Some((agent, root))
    }

    /// Clear a thread's draft after its prompt is sent.
    pub fn clear_draft(&mut self, agent_id: Uuid, cx: &mut App) {
        if let Some(field) = self.drafts.get(&agent_id).cloned() {
            field.update(cx, |field, cx| field.set_value(String::new(), cx));
        }
        self.restored_drafts.remove(&agent_id);
    }

    /// Whether a turn is running.
    pub fn is_busy(&self) -> bool {
        self.snapshot
            .active_agent_id
            .is_some_and(|id| self.runners.contains_key(&id))
    }

    pub fn any_busy(&self) -> bool {
        !self.runners.is_empty()
    }

    /// Ask the running turn to stop.
    pub fn stop_turn(&self) {
        if let Some(runner) = self
            .snapshot
            .active_agent_id
            .and_then(|id| self.runners.get(&id))
        {
            runner.interrupt();
        }
    }

    /// The active conversation without loading it.
    ///
    /// What a render path wants: it cannot mutate, and reading from disk mid-render
    /// would mean a view deciding when a thread's history is loaded.
    pub fn peek_active_conversation(&self) -> Option<Arc<GuardedConversation>> {
        let agent_id = self.snapshot.active_agent_id?;
        self.conversations.get(&agent_id).cloned()
    }

    /// Write the snapshot to disk.
    ///
    /// Called on quit and after any change that would be costly to lose.
    /// Errors are recorded rather than propagated: a failed save must not take
    /// the window down.
    pub fn save(&mut self) {
        if !self.snapshot_writable {
            // Launching with a broken snapshot is useful for diagnosis, but it
            // must not replace both recovery candidates with the empty fallback.
            return;
        }
        self.snapshot.sidebar_visible = self.sidebar_visible;
        self.snapshot.normalize();
        if serde_json::to_vec(&self.snapshot)
            .is_ok_and(|bytes| bytes.len() as u64 > fx_store::format::MAX_PROJECTS_BYTES)
        {
            self.last_error = Some("The workspace exceeds its metadata file size limit and could not be saved. The previous saved workspace has been preserved.".into());
            return;
        }
        match self.store.save_app(&self.snapshot) {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(%error, "could not save the project snapshot");
                self.last_error = Some(error.to_string());
            }
        }
    }

    fn capture_drafts(&mut self, cx: &App) {
        for (&agent_id, field) in &self.drafts {
            self.restored_drafts
                .insert(agent_id, field.read(cx).value().to_string());
        }
    }

    /// Queue saves for loaded conversations that changed. Unopened and
    /// unreadable siblings remain untouched.
    ///
    /// Returns at once. The UI thread only compares a revision and a draft; the
    /// copy, the encode, and the write all happen on the writer's thread.
    pub fn save_conversations(&mut self) {
        let loaded: Vec<_> = self
            .conversations
            .iter()
            .map(|(&id, value)| (id, value.clone()))
            .collect();
        for (agent_id, conversation) in loaded {
            if self.protected_conversations.contains(&agent_id) {
                continue;
            }
            let Some(project_id) = self.project_for_agent(agent_id) else {
                continue;
            };
            let draft = self
                .restored_drafts
                .get(&agent_id)
                .cloned()
                .unwrap_or_default();
            let revision = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .revision();
            let version = (revision, draft);
            if self.saved_conversations.get(&agent_id) == Some(&version) {
                continue;
            }
            self.writer.submit(crate::conversation_writer::Job {
                store: self.store.clone(),
                project_id,
                agent_id,
                conversation,
                draft: version.1.clone(),
                retry_draft: self.retry_drafts.get(&agent_id).cloned(),
                goal: self.composer.goals.get(&agent_id).cloned(),
                cost: self
                    .restored_costs
                    .get(&agent_id)
                    .copied()
                    .unwrap_or_default(),
            });
            self.saved_conversations.insert(agent_id, version);
        }
    }

    /// Report how queued saves went. Returns whether that changed anything the
    /// window shows.
    fn apply_save_outcomes(&mut self) -> bool {
        use crate::conversation_writer::Saved;
        let mut reported = false;
        for outcome in self.writer.take_outcomes() {
            if self.deleted_agents.contains(&outcome.agent_id) {
                continue;
            }
            match outcome.result {
                Ok(Saved::Partial) => {
                    self.last_error = Some("This conversation exceeds the local history cache limit. The complete history remains with the provider and in memory; its draft and session have been saved with the available cached history.".into());
                    reported = true;
                }
                Ok(Saved::Written | Saved::Unchanged | Saved::Removed) => {}
                Err(error) => {
                    tracing::error!(agent_id = %outcome.agent_id, %error, "could not save the conversation");
                    // Ask again at the next save rather than believing the
                    // failed version landed.
                    self.saved_conversations.remove(&outcome.agent_id);
                    self.last_error = Some(error);
                    reported = true;
                }
            }
        }
        reported
    }

    /// Wait for queued transcript saves to land, then report them.
    pub fn flush_saves(&mut self) {
        self.writer.flush();
        self.apply_save_outcomes();
    }

    /// Throttled recovery checkpoints while a turn streams or a draft changes.
    ///
    /// Returns whether a reported save changed what the window shows.
    pub fn checkpoint(&mut self, cx: &App) -> bool {
        let reported = self.apply_save_outcomes();
        if self.last_checkpoint.elapsed() < Duration::from_secs(1) {
            return reported;
        }
        self.last_checkpoint = Instant::now();
        self.capture_drafts(cx);
        self.save_conversations();
        reported
    }

    /// What a pause in typing saves: everything [`Self::save_all`] does,
    /// without waiting for transcripts to land.
    ///
    /// Returns whether a reported save changed what the window shows.
    pub fn autosave(&mut self, cx: &App) -> bool {
        let reported = self.apply_save_outcomes();
        self.capture_drafts(cx);
        self.save_conversations();
        self.save();
        reported
    }

    /// Save the whole loaded workspace, including drafts and partial streamed
    /// text, and wait for it to land. Quitting relies on the wait.
    pub fn save_all(&mut self, cx: &App) {
        self.capture_drafts(cx);
        self.save_conversations();
        self.flush_saves();
        self.save();
    }

    /// The root calls this on an idle-capable 500ms timer. Unit tests explicitly
    /// exercise result application instead of launching installed provider CLIs.
    pub fn native_sync_enabled(&self) -> bool {
        self.native_sync.allowed() && self.snapshot_writable && !self.snapshot.projects.is_empty()
    }

    pub fn native_history_loading(&self, agent_id: Uuid) -> bool {
        self.native_sync.history_loading(agent_id)
    }

    pub fn native_status(&self, project_id: Uuid) -> crate::native_sync::NativeSyncStatus {
        let mut warnings: Vec<_> = self
            .native_sync
            .warnings
            .iter()
            .filter(|((id, _), _)| *id == project_id)
            .flat_map(|(_, warnings)| warnings.clone())
            .collect();
        if let Some(agent) = self.snapshot.active_agent_id
            && self.project_for_agent(agent) == Some(project_id)
            && let Some(warning) = self.native_sync.history_warnings.get(&agent)
        {
            warnings.push(warning.clone());
        }
        warnings.sort();
        warnings.dedup();
        crate::native_sync::NativeSyncStatus {
            syncing: self.native_sync.pending_for(project_id),
            warnings,
        }
    }

    /// Everything a native-sync pass can change that the window shows.
    fn native_view(&self) -> NativeView {
        NativeView {
            snapshot: self.snapshot.clone(),
            warnings: self.native_sync.warnings.clone(),
            history_warnings: self.native_sync.history_warnings.clone(),
            revisions: self
                .conversations
                .iter()
                .map(|(id, conversation)| {
                    let revision = conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .revision();
                    (*id, revision)
                })
                .collect(),
        }
    }

    /// The loading states a running pass shows: "Loading provider threads…"
    /// for a project with no threads yet, and "Loading conversation history…"
    /// for a selected thread with nothing to show.
    fn native_loading(&self) -> Vec<bool> {
        let mut loading: Vec<bool> = self
            .snapshot
            .projects
            .iter()
            .filter(|record| record.agents.is_empty())
            .filter_map(|record| record.project.as_ref())
            .map(|project| self.native_sync.pending_for(project.id))
            .collect();
        loading.push(self.snapshot.active_agent_id.is_some_and(|agent| {
            self.native_sync.history_loading(agent)
                && self.conversations.get(&agent).is_none_or(|conversation| {
                    conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .message_count()
                        == 0
                })
        }));
        loading
    }

    pub fn request_native_refresh(&mut self, project_id: Uuid) {
        for provider in crate::native_sync::PROVIDERS {
            self.native_sync
                .index_due
                .remove(&(project_id, provider.into()));
        }
        if self.snapshot.active_project_id == Some(project_id)
            && let Some(agent) = self.snapshot.active_agent_id
        {
            self.native_sync.history_due.remove(&agent);
        }
    }

    /// Apply completed background work, then schedule only due requests. Returns
    /// whether the shell should redraw; waiting never forces continuous repaint.
    pub fn poll_native_sync(&mut self, cx: &App) -> bool {
        if !self.native_sync_enabled() {
            return false;
        }
        // A pass that found nothing new is not a change, and neither is
        // starting one. Reporting both redrew the whole window twice a pass —
        // every second or two — and flickered the sync row, while nothing on
        // screen had moved. This also runs on every render, so the full view
        // is compared only when a pass has finished.
        let loading_before = self.native_loading();
        let mut changed = false;
        let completions = self.native_sync.drain();
        if !completions.is_empty() {
            let before = self.native_view();
            for completion in completions {
                self.capture_drafts(cx);
                self.apply_native_completion(completion);
            }
            changed = self.native_view() != before;
        }
        let selected = self
            .snapshot
            .active_project_id
            .zip(self.snapshot.active_agent_id);
        if self.native_sync.selected != selected {
            self.native_sync.selected = selected;
            if let Some((_, agent)) = selected {
                self.native_sync.history_due.remove(&agent);
            }
        }
        let now = Instant::now();
        for provider in crate::native_sync::PROVIDERS {
            if !self.native_sync.ready(provider) {
                continue;
            }
            let active = self.snapshot.active_project_id;
            // Every saved project is discovered once. After that only the
            // active project polls; activation/explicit refresh makes others due.
            let mut projects: Vec<_> = self
                .snapshot
                .projects
                .iter()
                .filter_map(|record| record.project.as_ref())
                .collect();
            projects.sort_by_key(|project| Some(project.id) != active);
            let due_project = projects
                .into_iter()
                .find(|project| {
                    match self
                        .native_sync
                        .index_due
                        .get(&(project.id, provider.into()))
                    {
                        None => true,
                        Some(due) => Some(project.id) == active && now >= *due,
                    }
                })
                .map(|project| (project.id, project.root_path.clone()));
            let history_ticket = selected.and_then(|(project, agent)| {
                if self.is_thread_busy(agent)
                    || self
                        .native_sync
                        .completed_turns
                        .get(&agent)
                        .is_some_and(|finished| finished.elapsed() < Duration::from_secs(4))
                    || self
                        .native_sync
                        .history_due
                        .get(&agent)
                        .is_some_and(|due| now < *due)
                {
                    return None;
                }
                let root = self.project(project)?.root_path.clone();
                let binding = self
                    .snapshot
                    .projects
                    .iter()
                    .flat_map(|record| &record.native_bindings)
                    .find(|binding| binding.agent_id == agent && binding.provider_id == provider)?
                    .clone();
                // This runs from a render. A saved transcript is read by the
                // view that shows it; history waits for that read to land.
                let conversation = self.loaded_or_unsaved(project, agent)?;
                let revision = conversation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .revision();
                Some(crate::native_sync::Ticket {
                    project,
                    root,
                    provider: provider.into(),
                    generation: self.native_sync.generation(project),
                    kind: crate::native_sync::RequestKind::History {
                        agent,
                        session: binding.session_id,
                        revision,
                        known_revision: if binding.local_history_pending {
                            None
                        } else {
                            self.native_sync.loaded_revisions.get(&agent).cloned()
                        },
                    },
                })
            });
            let initial_history = selected
                .is_some_and(|(_, agent)| !self.native_sync.history_due.contains_key(&agent));
            let prefer_history = history_ticket.is_some()
                && (initial_history
                    || due_project.is_none()
                    || self
                        .native_sync
                        .last_was_index
                        .get(provider)
                        .copied()
                        .unwrap_or(false));
            let ticket = if prefer_history {
                history_ticket
            } else if let Some((project, root)) = due_project {
                let record = self
                    .snapshot
                    .projects
                    .iter()
                    .find(|record| record.project.as_ref().is_some_and(|p| p.id == project))
                    .unwrap();
                let candidates = record
                    .agents
                    .iter()
                    .filter(|agent| {
                        agent
                            .configuration
                            .provider_id
                            .as_deref()
                            .unwrap_or("codex")
                            == provider
                            && !record
                                .native_bindings
                                .iter()
                                .any(|binding| binding.agent_id == agent.id)
                    })
                    .map(|agent| agent.id)
                    .collect();
                let missing_candidates = record
                    .native_bindings
                    .iter()
                    .filter(|binding| binding.provider_id == provider)
                    .map(|binding| (binding.agent_id, binding.session_id.clone()))
                    .collect();
                self.native_sync.index_due.insert(
                    (project, provider.into()),
                    now + crate::native_sync::INDEX_INTERVAL,
                );
                Some(crate::native_sync::Ticket {
                    project,
                    root,
                    provider: provider.into(),
                    generation: self.native_sync.generation(project),
                    kind: crate::native_sync::RequestKind::Index {
                        candidates,
                        missing_candidates,
                    },
                })
            } else {
                history_ticket
            };
            if let Some(ticket) = ticket {
                let project = ticket.project;
                if let crate::native_sync::RequestKind::History { agent, .. } = ticket.kind {
                    self.native_sync
                        .history_due
                        .insert(agent, now + crate::native_sync::HISTORY_INTERVAL);
                }
                match self.native_sync.submit(ticket, self.paths.clone()) {
                    Ok(()) => {}
                    Err(error) => {
                        self.native_sync
                            .warnings
                            .insert((project, provider.into()), vec![error]);
                        changed = true;
                    }
                }
            }
        }
        changed || self.native_loading() != loading_before
    }

    fn apply_native_completion(&mut self, completion: crate::native_sync::Completion) -> bool {
        use crate::native_sync::{Payload, RequestKind, same_project};
        let ticket = completion.ticket;
        if self.native_sync.generation(ticket.project) != ticket.generation
            || !self
                .project(ticket.project)
                .is_some_and(|project| same_project(&project.root_path, &ticket.root))
        {
            return false;
        }
        match completion.result {
            Err(error) => match ticket.kind {
                RequestKind::Index { .. } => {
                    self.native_sync.warnings.insert(
                        (ticket.project, ticket.provider.clone()),
                        vec![format!("{} history unavailable: {error}", ticket.provider)],
                    );
                }
                RequestKind::History { agent, .. } => {
                    if self.project_for_agent(agent) == Some(ticket.project) {
                        self.native_sync.history_warnings.insert(agent, format!("Could not refresh native history: {error}. The cached conversation is preserved."));
                    }
                }
            },
            Ok(Payload::Index {
                index,
                sessions,
                protected,
            }) => {
                let mut warnings = index.warnings;
                let present: HashSet<_> = index
                    .threads
                    .iter()
                    .map(|entry| entry.summary.identity.session_id.clone())
                    .collect();
                let mut next = self.snapshot.clone();
                let record = next
                    .projects
                    .iter_mut()
                    .find(|record| {
                        record
                            .project
                            .as_ref()
                            .is_some_and(|project| project.id == ticket.project)
                    })
                    .unwrap();
                for entry in index.threads {
                    let identity = &entry.summary.identity;
                    if identity.provider_id != ticket.provider
                        || identity.session_id.is_empty()
                        || !same_project(&entry.summary.working_directory, &ticket.root)
                    {
                        warnings.push("A native history entry belonged to another provider or project and was ignored.".into());
                        continue;
                    }
                    if record.suppressed_native_threads.iter().any(|deleted| {
                        deleted.provider_id == identity.provider_id
                            && deleted.session_id == identity.session_id
                    }) {
                        continue;
                    }
                    let existing = record
                        .native_bindings
                        .iter()
                        .find(|binding| {
                            binding.provider_id == identity.provider_id
                                && binding.session_id == identity.session_id
                        })
                        .map(|binding| binding.agent_id)
                        .or_else(|| {
                            record
                                .agents
                                .iter()
                                .find(|agent| {
                                    if agent
                                        .configuration
                                        .provider_id
                                        .as_deref()
                                        .unwrap_or("codex")
                                        != identity.provider_id
                                    {
                                        return false;
                                    }
                                    self.conversations
                                        .get(&agent.id)
                                        .is_some_and(|conversation| {
                                            conversation
                                                .lock()
                                                .unwrap_or_else(|error| error.into_inner())
                                                .session_id()
                                                == Some(&identity.session_id)
                                        })
                                        || sessions.iter().any(|(id, session)| {
                                            *id == agent.id && *session == identity.session_id
                                        })
                                })
                                .map(|agent| agent.id)
                        });
                    let id = existing.unwrap_or_else(Uuid::new_v4);
                    record.missing_native_agent_ids.retain(|agent| *agent != id);
                    if self.is_thread_busy(id)
                        || self
                            .native_sync
                            .completed_turns
                            .get(&id)
                            .is_some_and(|finished| finished.elapsed() < Duration::from_secs(4))
                    {
                        continue;
                    }
                    if !record.agents.iter().any(|agent| agent.id == id) {
                        let mut agent = Agent::with_id(id, &entry.summary.title);
                        agent.configuration.provider_id = Some(identity.provider_id.clone());
                        record.agents.push(agent);
                        record.project.as_mut().unwrap().agent_order.push(id);
                    }
                    let overrides = record
                        .thread_overrides
                        .iter()
                        .find(|entry| entry.agent_id == id);
                    let title = overrides
                        .and_then(|entry| entry.title.clone())
                        .unwrap_or_else(|| entry.summary.title.clone());
                    let archived = overrides
                        .and_then(|entry| entry.archived)
                        .unwrap_or(entry.summary.is_archived);
                    let agent = record
                        .agents
                        .iter_mut()
                        .find(|agent| agent.id == id)
                        .unwrap();
                    agent.title = title;
                    // Native defaults stay in the binding, separate from explicit
                    // user model, effort, and access overrides.
                    agent
                        .configuration
                        .provider_id
                        .get_or_insert(identity.provider_id.clone());
                    record.archived_agent_ids.retain(|agent| *agent != id);
                    if archived {
                        record.archived_agent_ids.push(id);
                    }
                    let previous = record
                        .native_bindings
                        .iter()
                        .find(|binding| binding.agent_id == id)
                        .cloned();
                    record
                        .native_bindings
                        .retain(|binding| binding.agent_id != id);
                    let mut binding = native_binding(id, &entry);
                    if let Some(previous) = previous {
                        binding.local_history_pending = previous.local_history_pending;
                        binding.model = binding.model.or(previous.model);
                        binding.effort = binding.effort.or(previous.effort);
                        binding.agent_access = binding.agent_access.or(previous.agent_access);
                        binding.context_window_size =
                            binding.context_window_size.or(previous.context_window_size);
                    }
                    record.native_bindings.push(binding);
                }
                if warnings.is_empty() {
                    for binding in record
                        .native_bindings
                        .iter()
                        .filter(|binding| binding.provider_id == ticket.provider)
                    {
                        let id = binding.agent_id;
                        let protected = protected.contains(&id)
                            || self.is_thread_busy(id)
                            || self.snapshot.active_agent_id == Some(id)
                            || binding.local_history_pending
                            || self
                                .restored_drafts
                                .get(&id)
                                .is_some_and(|draft| !draft.is_empty())
                            || self.conversations.get(&id).is_some_and(|conversation| {
                                conversation
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .unsent()
                                    .is_some()
                            });
                        if !present.contains(&binding.session_id)
                            && !protected
                            && !record.missing_native_agent_ids.contains(&id)
                        {
                            record.missing_native_agent_ids.push(id);
                        }
                    }
                }
                if next.active_project_id == Some(ticket.project) && next.active_agent_id.is_none()
                {
                    next.active_agent_id = visible_agent(record, record.last_selected_agent_id);
                }
                // Provider errors and no-op polls must not rewrite the workspace.
                if serde_json::to_vec(&next).ok() != serde_json::to_vec(&self.snapshot).ok() {
                    self.commit_thread_change(next, "save native thread discovery");
                }
                self.native_sync
                    .warnings
                    .insert((ticket.project, ticket.provider), warnings);
            }
            Ok(Payload::Unchanged) => {
                if let RequestKind::History { agent, .. } = ticket.kind {
                    self.native_sync.history_warnings.remove(&agent);
                }
            }
            Ok(Payload::History(history)) => {
                let RequestKind::History {
                    agent,
                    session,
                    revision,
                    ..
                } = ticket.kind
                else {
                    return false;
                };
                if self.project_for_agent(agent) != Some(ticket.project)
                    || self.is_thread_busy(agent)
                {
                    return false;
                }
                let Some(binding) = self
                    .snapshot
                    .projects
                    .iter()
                    .flat_map(|record| &record.native_bindings)
                    .find(|binding| {
                        binding.agent_id == agent
                            && binding.provider_id == ticket.provider
                            && binding.session_id == session
                    })
                    .cloned()
                else {
                    return false;
                };
                if history.entry.summary.identity.provider_id != ticket.provider
                    || history.entry.summary.identity.session_id != session
                    || !same_project(&history.entry.summary.working_directory, &ticket.root)
                {
                    self.native_sync.history_warnings.insert(agent, "Native history did not match this thread's project and provider; the cache was preserved.".into());
                    return true;
                }
                if history.history_truncated {
                    self.native_sync.history_warnings.insert(agent, "The provider returned incomplete history. The cached conversation is preserved.".into());
                    return true;
                }
                if self.protected_conversations.contains(&agent) {
                    self.native_sync.history_warnings.insert(agent, "The local conversation cache could not be read. Its files and draft are preserved.".into());
                    return true;
                }
                let conversation = self.conversation(ticket.project, agent);
                {
                    let mut conversation = conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if conversation.revision() != revision
                        || conversation.is_busy()
                        || conversation.unsent().is_some()
                    {
                        return false;
                    }
                    if history.messages.is_empty() && conversation.message_count() > 0 {
                        self.native_sync.history_warnings.insert(agent, "The provider returned empty history for a cached conversation. The local history is preserved.".into());
                        return true;
                    }
                    if binding.local_history_pending
                        && !native_contains_local_tail(&conversation, &history.messages)
                    {
                        self.native_sync.history_warnings.insert(agent, "The provider's history has not caught up with the latest turn. The local conversation is preserved.".into());
                        return true;
                    }
                    if !conversation.messages().eq(history.messages.iter()) {
                        conversation.replace_full_history(history.messages);
                    }
                    if let Some(tokens) = history.current_context_tokens
                        && tokens != conversation.usage().current_context_tokens
                    {
                        let mut usage = conversation.usage();
                        usage.current_context_tokens = tokens;
                        conversation.restore_usage(usage);
                    }
                }
                let metadata_before = serde_json::to_vec(&binding).ok();
                let mut metadata_changed = false;
                for binding in self
                    .snapshot
                    .projects
                    .iter_mut()
                    .flat_map(|record| &mut record.native_bindings)
                    .filter(|binding| binding.agent_id == agent)
                {
                    binding.local_history_pending = false;
                    binding.model = history
                        .entry
                        .defaults
                        .model_id
                        .clone()
                        .or(binding.model.clone());
                    binding.effort = history
                        .entry
                        .defaults
                        .effort
                        .clone()
                        .or(binding.effort.clone());
                    binding.agent_access =
                        history.entry.defaults.agent_access.or(binding.agent_access);
                    binding.context_window_size = history
                        .entry
                        .defaults
                        .context_window_size
                        .or(binding.context_window_size);
                    metadata_changed = serde_json::to_vec(binding).ok() != metadata_before;
                }
                self.native_sync.history_warnings.remove(&agent);
                self.native_sync
                    .loaded_revisions
                    .insert(agent, history.entry.revision);
                self.save_conversations();
                if metadata_changed {
                    self.save();
                }
            }
        }
        true
    }

    /// Adopt a Rust-created session before index discovery, so the same provider
    /// session can never appear as a second sidebar thread.
    fn bind_local_session(&mut self, agent_id: Uuid) {
        let Some(project_id) = self.project_for_agent(agent_id) else {
            return;
        };
        let conversation = self.conversation(project_id, agent_id);
        let session = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .session_id()
            .map(str::to_owned);
        let Some(session) = session else {
            return;
        };
        let record = self
            .snapshot
            .projects
            .iter_mut()
            .find(|record| {
                record
                    .project
                    .as_ref()
                    .is_some_and(|project| project.id == project_id)
            })
            .unwrap();
        if record
            .native_bindings
            .iter()
            .any(|binding| binding.agent_id == agent_id)
        {
            return;
        }
        let agent = record
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .unwrap();
        record.native_bindings.push(fx_store::PersistedBinding {
            agent_id,
            provider_id: agent
                .configuration
                .provider_id
                .clone()
                .unwrap_or_else(|| "codex".into()),
            session_id: session,
            title: agent.title.clone(),
            working_directory: record.project.as_ref().unwrap().root_path.clone(),
            updated_at: fx_core::models::now(),
            model: None,
            effort: None,
            agent_access: None,
            is_archived: record.archived_agent_ids.contains(&agent_id),
            revision: String::new(),
            context_window_size: None,
            local_history_pending: false,
        });
    }

    fn mark_native_local_turn(&mut self, agent_id: Uuid) {
        let Some(project) = self.project_for_agent(agent_id) else {
            return;
        };
        self.bind_local_session(agent_id);
        for binding in self
            .snapshot
            .projects
            .iter_mut()
            .flat_map(|record| &mut record.native_bindings)
            .filter(|binding| binding.agent_id == agent_id)
        {
            binding.local_history_pending = true;
        }
        self.native_sync.invalidate(project);
        self.native_sync
            .completed_turns
            .insert(agent_id, Instant::now());
        self.save();
    }

    pub fn native_defaults(&self, agent_id: Uuid) -> fx_core::NativeDefaults {
        self.snapshot
            .projects
            .iter()
            .flat_map(|record| &record.native_bindings)
            .find(|binding| binding.agent_id == agent_id)
            .map(|binding| fx_core::NativeDefaults {
                provider_id: Some(binding.provider_id.clone()),
                model_id: binding.model.clone(),
                effort: binding.effort.clone(),
                agent_access: binding.agent_access,
                context_window_size: binding.context_window_size,
            })
            .unwrap_or_default()
    }

    /// The current design settings.
    pub fn theme(&self) -> Theme {
        global::current()
    }

    pub fn set_theme(&mut self, theme: Theme) {
        global::set(theme);
    }

    /// Rotate the accent, for the gallery-style control.
    pub fn cycle_accent(&mut self) {
        let theme = self.theme();
        let next = theme.accent_color.next();
        self.set_theme(theme.with_accent(next));
    }

    pub fn cycle_appearance(&mut self) {
        let theme = self.theme();
        self.set_theme(theme.with_appearance(theme.appearance_mode.next()));
    }

    pub fn cycle_tone(&mut self) {
        let theme = self.theme();
        self.set_theme(theme.with_tone(theme.base_tone.next()));
    }

    pub fn cycle_text_size(&mut self) {
        let theme = self.theme();
        self.set_theme(theme.with_text_size(theme.text_size.next()));
    }
}

/// Most prompts one thread may queue, and most image bytes they may hold
/// together. The original's limits.
pub const MAX_QUEUED_PROMPTS: usize = 20;
pub const MAX_QUEUED_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;

/// What each thread's composer holds besides its text, and what is in flight
/// on its behalf.
///
/// One owner for all of it rather than a map per concern on [`AppState`]:
/// a thread's images, its queue's images, its unanswered steer and its goal
/// all live and die with the thread, and are read together by one view.
#[derive(Default)]
pub struct ComposerStore {
    attachments: HashMap<Uuid, Vec<fx_core::Attachment>>,
    preparing: HashMap<Uuid, usize>,
    notices: HashMap<Uuid, String>,
    /// Images of an unaccepted prompt, put back with its text.
    retry_attachments: HashMap<Uuid, Vec<fx_core::Attachment>>,
    /// Images travelling with a queued prompt, by the queued prompt's id.
    queued_attachments: HashMap<Uuid, Vec<fx_core::Attachment>>,
    steers: HashMap<Uuid, PendingSteer>,
    accepted_steers: Vec<(Uuid, PendingSteer)>,
    goals: HashMap<Uuid, fx_core::models::ConversationGoal>,
    /// An objective `/goal` set, to send as a prompt once the goal is in place.
    goal_prompts: HashMap<Uuid, String>,
    /// Threads whose turn ended with prompts waiting behind it.
    dispatch: Vec<Uuid>,
    /// The queued prompt being edited in place, if any.
    pub editor: Option<QueueEditor>,
}

/// Guidance sent into a running turn and not yet answered.
#[derive(Debug, Clone)]
pub struct PendingSteer {
    pub id: Uuid,
    pub text: String,
    pub attachment_ids: Vec<Uuid>,
}

/// A queued prompt open for editing, and the field editing it.
#[derive(Clone)]
pub struct QueueEditor {
    pub agent_id: Uuid,
    pub queued_id: Uuid,
    pub field: Entity<TextField>,
}

/// What a submitted draft becomes while a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowUp {
    /// Guide the running turn now. The original's default.
    #[default]
    Steer,
    /// Run after the running turn.
    Queue,
}

impl FollowUp {
    pub fn from_preference(value: &str) -> Self {
        if value.eq_ignore_ascii_case("queue") {
            Self::Queue
        } else {
            Self::Steer
        }
    }

    pub fn preference(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Self::Steer => Self::Queue,
            Self::Queue => Self::Steer,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Steer => "Steer",
            Self::Queue => "Queue",
        }
    }
}

impl AppState {
    // ── Composer attachments ───────────────────────────────────────────────

    /// The images waiting in a thread's composer.
    pub fn attachments(&self, agent_id: Uuid) -> &[fx_core::Attachment] {
        self.composer
            .attachments
            .get(&agent_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Whether images are still being read for this thread.
    pub fn is_preparing_attachments(&self, agent_id: Uuid) -> bool {
        self.composer
            .preparing
            .get(&agent_id)
            .is_some_and(|count| *count > 0)
    }

    /// What the composer should say about its last attach, send, or steer.
    pub fn composer_notice(&self, agent_id: Uuid) -> Option<&str> {
        self.composer.notices.get(&agent_id).map(String::as_str)
    }

    pub fn set_composer_notice(&mut self, agent_id: Uuid, notice: Option<String>) {
        match notice {
            Some(notice) => self.composer.notices.insert(agent_id, notice),
            None => self.composer.notices.remove(&agent_id),
        };
    }

    /// Note that a batch of images is being read for this thread.
    pub fn begin_preparing_attachments(&mut self, agent_id: Uuid) {
        *self.composer.preparing.entry(agent_id).or_default() += 1;
    }

    /// Admit a prepared batch under the composer's limits.
    pub fn finish_preparing_attachments(
        &mut self,
        agent_id: Uuid,
        prepared: crate::attachments::Prepared,
    ) {
        if let Some(count) = self.composer.preparing.get_mut(&agent_id) {
            *count = count.saturating_sub(1);
        }
        if self.deleted_agents.contains(&agent_id) {
            return;
        }
        let pending = self.composer.attachments.entry(agent_id).or_default();
        let notice = crate::attachments::admit(pending, prepared);
        self.set_composer_notice(agent_id, notice);
    }

    /// Attach an image taken from the clipboard.
    pub fn attach_pasted(&mut self, agent_id: Uuid, image: &gpui::Image) {
        let prepared = match crate::attachments::prepare_pasted(image) {
            Ok(attachment) => crate::attachments::Prepared {
                attachments: vec![attachment],
                errors: Vec::new(),
            },
            Err(error) => crate::attachments::Prepared {
                attachments: Vec::new(),
                errors: vec![error],
            },
        };
        self.begin_preparing_attachments(agent_id);
        self.finish_preparing_attachments(agent_id, prepared);
    }

    pub fn remove_attachment(&mut self, agent_id: Uuid, attachment_id: Uuid) {
        if let Some(pending) = self.composer.attachments.get_mut(&agent_id) {
            pending.retain(|attachment| attachment.id != attachment_id);
        }
    }

    /// Drop the composer's images that went out with a prompt, keeping any
    /// attached since.
    fn release_sent_attachments(&mut self, agent_id: Uuid, sent: &[Uuid]) {
        if let Some(pending) = self.composer.attachments.get_mut(&agent_id) {
            pending.retain(|attachment| !sent.contains(&attachment.id));
        }
    }

    // ── Queue ──────────────────────────────────────────────────────────────

    /// Queue a prompt behind the running turn, with its images.
    ///
    /// The composer is cleared only once the queue has taken the prompt, so a
    /// refusal leaves the reader's words and images where they were.
    pub fn enqueue_prompt(
        &mut self,
        agent_id: Uuid,
        text: &str,
        attachments: Vec<fx_core::Attachment>,
        cx: &mut App,
    ) -> Result<(), String> {
        let conversation = self
            .conversation_by_agent(agent_id)
            .ok_or("This thread is not loaded.")?;
        let queued: Vec<Uuid> = {
            let conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            conversation.queued().map(|prompt| prompt.id).collect()
        };
        if queued.len() >= MAX_QUEUED_PROMPTS {
            return Err(format!(
                "The queue is full (maximum {MAX_QUEUED_PROMPTS} prompts)."
            ));
        }
        let queued_bytes: usize = queued
            .iter()
            .filter_map(|id| self.composer.queued_attachments.get(id))
            .flatten()
            .map(|attachment| attachment.data.len())
            .sum();
        let bytes: usize = attachments
            .iter()
            .map(|attachment| attachment.data.len())
            .sum();
        if queued_bytes + bytes > MAX_QUEUED_ATTACHMENT_BYTES {
            return Err("Queued attachments would exceed the 64 MB per-conversation limit.".into());
        }
        let id = Uuid::new_v4();
        let sent: Vec<Uuid> = attachments.iter().map(|attachment| attachment.id).collect();
        {
            let mut conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            conversation.enqueue(fx_agent::conversation::QueuedPrompt {
                id,
                text: text.to_string(),
                attachment_count: attachments.len(),
            });
        }
        if !attachments.is_empty() {
            self.composer.queued_attachments.insert(id, attachments);
        }
        self.release_sent_attachments(agent_id, &sent);
        if let Some(field) = self.drafts.get(&agent_id).cloned()
            && field.read(cx).value().trim() == text.trim()
        {
            self.clear_draft(agent_id, cx);
        }
        self.set_composer_notice(agent_id, None);
        Ok(())
    }

    /// Remove a queued prompt and the images it carried.
    pub fn remove_queued(&mut self, agent_id: Uuid, index: usize) {
        let Some(conversation) = self.conversation_by_agent(agent_id) else {
            return;
        };
        let removed = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove_queued_at(index);
        if let Some(removed) = removed {
            self.composer.queued_attachments.remove(&removed.id);
            if self
                .composer
                .editor
                .as_ref()
                .is_some_and(|editor| editor.queued_id == removed.id)
            {
                self.composer.editor = None;
            }
        }
    }

    /// Open a queued prompt for editing in place.
    pub fn begin_editing_queued(&mut self, agent_id: Uuid, index: usize, cx: &mut App) {
        let Some(conversation) = self.conversation_by_agent(agent_id) else {
            return;
        };
        let Some(queued) = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .queued_at(index)
            .cloned()
        else {
            return;
        };
        let field = cx.new(|cx| {
            let mut field = TextField::new(cx)
                .placeholder("Edit queued prompt")
                .multiline(6.0)
                .max_len(32_000);
            field.set_value(queued.text.clone(), cx);
            field
        });
        self.composer.editor = Some(QueueEditor {
            agent_id,
            queued_id: queued.id,
            field,
        });
    }

    pub fn queue_editor(&self) -> Option<&QueueEditor> {
        self.composer.editor.as_ref()
    }

    pub fn cancel_editing_queued(&mut self) {
        self.composer.editor = None;
    }

    /// Save the open edit. An edit that leaves only whitespace is refused and
    /// the editor stays open: emptying a prompt is what Remove is for.
    pub fn save_queued_edit(&mut self, cx: &App) -> bool {
        let Some(editor) = self.composer.editor.clone() else {
            return false;
        };
        let text = editor.field.read(cx).value().to_string();
        let Some(conversation) = self.conversation_by_agent(editor.agent_id) else {
            self.composer.editor = None;
            return false;
        };
        let saved = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .update_queued(editor.queued_id, &text);
        if saved {
            self.composer.editor = None;
        }
        saved
    }

    /// Threads ready to send the next queued prompt, taken once.
    pub fn take_ready_queues(&mut self) -> Vec<Uuid> {
        std::mem::take(&mut self.composer.dispatch)
    }

    /// The next queued prompt for a thread, with its images.
    pub fn dequeue_prompt(&mut self, agent_id: Uuid) -> Option<(String, Vec<fx_core::Attachment>)> {
        if self.runners.contains_key(&agent_id) {
            return None;
        }
        let conversation = self.conversation_by_agent(agent_id)?;
        let queued = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dequeue()?;
        if self
            .composer
            .editor
            .as_ref()
            .is_some_and(|editor| editor.queued_id == queued.id)
        {
            self.composer.editor = None;
        }
        let attachments = self
            .composer
            .queued_attachments
            .remove(&queued.id)
            .unwrap_or_default();
        Some((queued.text, attachments))
    }

    /// Note that a thread's turn has ended, so its queue may move.
    ///
    /// Not after a prompt the provider never accepted: the next one would
    /// meet the same refusal, which is the original's rule.
    fn queue_may_advance(&mut self, agent_id: Uuid) {
        let Some(conversation) = self.conversations.get(&agent_id) else {
            return;
        };
        let conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if conversation.unsent().is_none()
            && conversation.queued_count() > 0
            && !self.composer.dispatch.contains(&agent_id)
        {
            self.composer.dispatch.push(agent_id);
        }
    }

    // ── Steering ───────────────────────────────────────────────────────────

    /// Send guidance into the running turn. The draft stays in the composer
    /// until the provider accepts it.
    pub fn begin_steer(
        &mut self,
        agent_id: Uuid,
        text: &str,
        attachments: Vec<fx_core::Attachment>,
    ) -> Result<(), String> {
        if self.composer.steers.contains_key(&agent_id) {
            return Err("Wait for the previous guidance to be accepted.".into());
        }
        let Some(runner) = self.runners.get(&agent_id) else {
            return Err(
                "There is no active turn to steer. Send this as a new prompt or queue it instead."
                    .into(),
            );
        };
        let steer = PendingSteer {
            id: Uuid::new_v4(),
            text: text.to_string(),
            attachment_ids: attachments.iter().map(|attachment| attachment.id).collect(),
        };
        if !runner.steer(steer.id, text.to_string(), attachments) {
            return Err(
                "The turn ended before the guidance could be sent. Send it as a new prompt.".into(),
            );
        }
        self.composer.steers.insert(agent_id, steer);
        self.set_composer_notice(agent_id, None);
        Ok(())
    }

    /// Whether guidance for this thread is waiting on the provider.
    pub fn is_steering(&self, agent_id: Uuid) -> bool {
        self.composer.steers.contains_key(&agent_id)
    }

    fn steer_answered(&mut self, agent_id: Uuid, id: Uuid, result: Result<(), String>) {
        if self
            .composer
            .steers
            .get(&agent_id)
            .is_none_or(|steer| steer.id != id)
        {
            return;
        }
        let Some(steer) = self.composer.steers.remove(&agent_id) else {
            return;
        };
        match result {
            Ok(()) => self.composer.accepted_steers.push((agent_id, steer)),
            Err(reason) => self.set_composer_notice(
                agent_id,
                Some(format!(
                    "Could not steer the active turn: {reason} Your message was not sent; retry or queue it."
                )),
            ),
        }
    }

    /// Clear accepted guidance from the composer, unless it has been edited.
    fn clear_accepted_steers(&mut self, cx: &mut App) {
        for (agent_id, steer) in std::mem::take(&mut self.composer.accepted_steers) {
            if let Some(field) = self.drafts.get(&agent_id).cloned()
                && field.read(cx).value().trim() == steer.text.trim()
            {
                self.clear_draft(agent_id, cx);
            }
            self.release_sent_attachments(agent_id, &steer.attachment_ids);
        }
    }

    // ── Thread controls and goals ──────────────────────────────────────────

    /// The thread's goal, as the provider last reported it.
    pub fn goal(&self, agent_id: Uuid) -> Option<&fx_core::models::ConversationGoal> {
        self.composer.goals.get(&agent_id)
    }

    /// Whether a thread may start a control now, and its conversation if so.
    pub fn prepare_control(&mut self, agent_id: Uuid) -> Option<Arc<GuardedConversation>> {
        if self.runners.contains_key(&agent_id) || self.protected_conversations.contains(&agent_id)
        {
            return None;
        }
        let project_id = self.project_for_agent(agent_id)?;
        let conversation = self.conversation(project_id, agent_id);
        if conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_busy()
        {
            return None;
        }
        Some(conversation)
    }

    /// Send this objective as a prompt once `/goal` has set it.
    pub fn send_after_goal(&mut self, agent_id: Uuid, objective: String) {
        self.composer.goal_prompts.insert(agent_id, objective);
    }

    fn goal_changed(&mut self, agent_id: Uuid, goal: Option<fx_core::models::ConversationGoal>) {
        match goal {
            Some(goal) => {
                if let Some(objective) = self.composer.goal_prompts.remove(&agent_id)
                    && let Some(conversation) = self.conversations.get(&agent_id)
                {
                    conversation
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .enqueue(fx_agent::conversation::QueuedPrompt {
                            id: Uuid::new_v4(),
                            text: objective,
                            attachment_count: 0,
                        });
                }
                self.composer.goals.insert(agent_id, goal);
            }
            None => {
                self.composer.goals.remove(&agent_id);
            }
        }
        // The goal is not part of the conversation's revision, so the
        // unchanged-version shortcut would skip writing it.
        self.saved_conversations.remove(&agent_id);
    }

    /// Fill in a Claude thread's context from the CLI when FlowX has none.
    ///
    /// A thread saved before Claude's usage was recorded, or one first seen
    /// through native history, has no window size: Claude only reports it at
    /// the end of a turn. Asking the CLI once, in the background, shows the
    /// meter when the thread is opened rather than after the next reply.
    pub fn probe_context_if_unknown(&mut self, agent_id: Uuid, cx: &mut Context<Self>) {
        let Some((session, root)) = self.context_probe_target(agent_id) else {
            return;
        };
        self.context_probed.insert(agent_id);
        let check = cx.background_spawn(async move {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(fx_agent::claude::catalog::session_context(
                        &fx_core::services::runtime::RuntimeDiscovery::default(),
                        &root,
                        &session,
                    ))
                })
                .map(|context| (session, context))
        });
        cx.spawn(async move |this, cx| {
            let Ok((session, context)) = check.await else {
                return;
            };
            let _ = this.update(cx, |state, cx| {
                state.apply_probed_context(agent_id, &session, &context);
                cx.notify();
            });
        })
        .detach();
    }

    /// The session and folder to ask about, when a thread needs its context
    /// filled in: a Claude thread, idle, with a session, whose meter has
    /// nothing to show, and not already asked this launch.
    fn context_probe_target(&self, agent_id: Uuid) -> Option<(String, String)> {
        if self.context_probed.contains(&agent_id) || self.is_thread_busy(agent_id) {
            return None;
        }
        let agent = self.thread(agent_id)?;
        if crate::thread_presentation::provider(self, agent)
            != fx_design::components::ProviderIdentity::Claude
        {
            return None;
        }
        let conversation = self.conversations.get(&agent_id)?;
        let conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let known = crate::thread_presentation::context_usage(self, agent, Some(&conversation))
            .fraction()
            .is_some();
        if known {
            return None;
        }
        let session = conversation.session_id()?.to_owned();
        Some((session, self.project_root_for(agent_id)?))
    }

    /// Record a context check, unless the thread has moved on since it was
    /// asked: a turn that ran meanwhile reported fresher figures itself.
    fn apply_probed_context(
        &mut self,
        agent_id: Uuid,
        session: &str,
        context: &fx_agent::claude::catalog::SessionContext,
    ) {
        if self.is_thread_busy(agent_id) {
            return;
        }
        let Some(conversation) = self.conversations.get(&agent_id) else {
            return;
        };
        let mut conversation = conversation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if conversation.session_id() == Some(session)
            && conversation
                .usage()
                .reported_context_window
                .is_none_or(|window| window == 0)
        {
            conversation.record_context(context.used, context.window);
        }
    }

    /// The folder a thread's project lives in.
    pub fn project_root_for(&self, agent_id: Uuid) -> Option<String> {
        let project_id = self.project_for_agent(agent_id)?;
        self.project(project_id)
            .map(|project| project.root_path.clone())
    }

    /// Put a prompt that could not start back where the reader can see it.
    pub fn return_to_composer(
        &mut self,
        agent_id: Uuid,
        text: String,
        attachments: Vec<fx_core::Attachment>,
        cx: &mut Context<Self>,
    ) {
        let field = self.draft(agent_id, cx);
        if field.read(cx).value().trim().is_empty() && self.attachments(agent_id).is_empty() {
            field.update(cx, |field, cx| field.set_value(text, cx));
            self.composer.attachments.insert(agent_id, attachments);
        } else {
            self.set_composer_notice(
                agent_id,
                Some(format!("A queued prompt could not be sent: {text}")),
            );
        }
    }

    // ── Follow-up mode ─────────────────────────────────────────────────────

    /// What ⌘↩ does while a turn runs. ⌃↩ does the other.
    pub fn follow_up_mode(&self) -> FollowUp {
        FollowUp::from_preference(&self.preferences.follow_up_mode)
    }
}

/// Keep local-only drafts and resume metadata even when native history cannot
/// fit the bounded recovery cache. The caller reports this partial cache.
///
/// Measured in the encoding that is written, because that is what a later read
/// is bounded by.
pub(crate) fn encode_partial_cache(
    mut record: PersistedConversation,
    previous: Option<PersistedConversation>,
    limit: u64,
) -> Result<(PersistedConversation, fx_store::EncodedConversation), String> {
    let encode = |record: &PersistedConversation| {
        fx_store::EncodedConversation::new(record).map_err(|error| error.to_string())
    };
    record.messages = previous.map(|old| old.messages).unwrap_or_default();
    let mut bytes = encode(&record)?;
    if bytes.len() as u64 > limit {
        record.messages.clear();
        bytes = encode(&record)?;
    }
    if bytes.len() as u64 > limit {
        return Err(
            "The unsent draft itself exceeds the local cache limit and could not be saved.".into(),
        );
    }
    Ok((record, bytes))
}

fn local_override(
    record: &mut fx_store::PersistedProject,
    agent_id: Uuid,
) -> &mut fx_store::format::PersistedThreadOverride {
    if let Some(index) = record
        .thread_overrides
        .iter()
        .position(|entry| entry.agent_id == agent_id)
    {
        return &mut record.thread_overrides[index];
    }
    record
        .thread_overrides
        .push(fx_store::format::PersistedThreadOverride {
            agent_id,
            title: None,
            archived: None,
        });
    record.thread_overrides.last_mut().unwrap()
}

fn native_binding(
    agent_id: Uuid,
    entry: &fx_agent::native::NativeThreadSummary,
) -> fx_store::PersistedBinding {
    fx_store::PersistedBinding {
        agent_id,
        provider_id: entry.summary.identity.provider_id.clone(),
        session_id: entry.summary.identity.session_id.clone(),
        title: entry.summary.title.clone(),
        working_directory: entry.summary.working_directory.clone(),
        updated_at: entry.summary.updated_at,
        model: entry.defaults.model_id.clone(),
        effort: entry.defaults.effort.clone(),
        agent_access: entry.defaults.agent_access,
        is_archived: entry.summary.is_archived,
        revision: entry.revision.clone(),
        context_window_size: entry.defaults.context_window_size,
        local_history_pending: false,
    }
}

/// Native files may lag a successfully completed pipe turn. Do not exchange a
/// known good local tail for an older file merely because a cooldown elapsed.
fn native_contains_local_tail(
    conversation: &Conversation,
    messages: &[fx_core::ConversationMessage],
) -> bool {
    // Providers may combine or split assistant prose around tools differently.
    // Compare normalized, consecutive role blocks, consuming each native block
    // once. Checking only the final text anywhere in history lets an earlier
    // identical "continue" / "Done." turn masquerade as the just-finished one.
    fn prose_blocks<'a>(
        messages: impl Iterator<Item = &'a fx_core::ConversationMessage>,
    ) -> Vec<(fx_core::MessageRole, String)> {
        let mut blocks: Vec<(fx_core::MessageRole, String)> = Vec::new();
        for message in messages {
            if !matches!(
                message.role,
                fx_core::MessageRole::User | fx_core::MessageRole::Assistant
            ) {
                continue;
            }
            let text = message
                .text_content()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                continue;
            }
            if let Some((role, previous)) = blocks.last_mut()
                && *role == message.role
            {
                previous.push(' ');
                previous.push_str(&text);
            } else {
                blocks.push((message.role, text));
            }
        }
        blocks
    }

    let mut native = prose_blocks(messages.iter()).into_iter();
    prose_blocks(conversation.messages())
        .into_iter()
        .all(|(role, text)| {
            native.any(|(native_role, native_text)| {
                native_role == role && native_text.contains(&text)
            })
        })
}

/// The images of a conversation's unsent prompt, read back from its user row.
fn unsent_attachments(conversation: &Conversation) -> Vec<fx_core::Attachment> {
    let Some(unsent) = conversation.unsent() else {
        return Vec::new();
    };
    conversation
        .messages()
        .last()
        .filter(|message| {
            message.role == fx_core::MessageRole::User && message.text_content() == unsent.text
        })
        .map(crate::attachments::attachments_in)
        .unwrap_or_default()
}

/// Whether two image lists carry the same bytes in the same order.
fn same_images(left: &[fx_core::Attachment], right: &[fx_core::Attachment]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.data == right.data)
}

/// Copy of a conversation for persistence.
pub fn persisted_from(conversation: &Conversation) -> PersistedConversation {
    let mut persisted = PersistedConversation::from_messages(
        conversation.agent_id,
        conversation.messages().cloned().collect(),
    );
    // Snapshot in-flight text without committing into the live buffer: later
    // deltas must continue the same segment and must not duplicate it.
    if !conversation.streaming_text().is_empty() {
        persisted.messages.push(fx_core::ConversationMessage::text(
            fx_core::MessageRole::Assistant,
            conversation.streaming_text(),
        ));
    }
    persisted.session_id = conversation.session_id().map(str::to_string);
    let usage = conversation.usage();
    persisted.total_input_tokens = usage.input_tokens;
    persisted.total_output_tokens = usage.output_tokens;
    persisted.total_cached_input_tokens = usage.cached_input_tokens;
    persisted.total_reasoning_output_tokens = usage.reasoning_output_tokens;
    persisted.total_tokens = usage.total_tokens;
    persisted.current_context_tokens = usage.current_context_tokens;
    persisted.reported_context_window = usage.reported_context_window;
    persisted.unsent_prompt = conversation.unsent().map(|prompt| {
        fx_store::PersistedUnsentPrompt::new(
            prompt.text.clone(),
            prompt.attachment_count,
            prompt.session_id.clone(),
        )
    });
    persisted
}

/// What a native-sync pass can change on screen, compared before and after it
/// is applied.
#[derive(PartialEq)]
struct NativeView {
    snapshot: PersistedApp,
    warnings: HashMap<(Uuid, String), Vec<String>>,
    history_warnings: HashMap<Uuid, String>,
    revisions: HashMap<Uuid, u64>,
}

/// What reading a thread's saved transcript produced.
///
/// Built entirely from the store, so a read and parse of tens of megabytes can
/// happen off the UI thread; installing it is all the UI thread does.
pub(crate) struct LoadedConversation {
    conversation: Conversation,
    /// The saved cost and draft, when there was a readable file.
    restored: Option<(f64, String)>,
    /// Why the saved files could not be used, when they could not.
    failure: Option<String>,
    /// The thread's saved goal.
    goal: Option<fx_core::models::ConversationGoal>,
    /// The unsent prompt's images, when the draft is that prompt.
    attachments: Vec<fx_core::Attachment>,
}

impl LoadedConversation {
    fn empty(agent_id: Uuid) -> Self {
        Self {
            conversation: Conversation::new(agent_id),
            restored: None,
            failure: None,
            goal: None,
            attachments: Vec::new(),
        }
    }
}

fn read_conversation(store: &Store, project_id: Uuid, agent_id: Uuid) -> LoadedConversation {
    let mut loaded = LoadedConversation::empty(agent_id);
    match store.load_conversation(project_id, agent_id) {
        Ok(Some(persisted)) if persisted.agent_id == agent_id => {
            // Whitespace is not a draft. A composer holding only a newline
            // must not stand in for the prompt that failed.
            let draft = if persisted.draft_text.trim().is_empty() {
                persisted
                    .unsent_prompt
                    .as_ref()
                    .map(|prompt| prompt.text.clone())
                    .unwrap_or_default()
            } else {
                persisted.draft_text.clone()
            };
            // The images come back when the draft is the unsent prompt —
            // restored from it, or already put back after its failure.
            let recover_images = persisted.unsent_prompt.as_ref().is_some_and(|prompt| {
                prompt.attachment_count > 0
                    && (persisted.draft_text.trim().is_empty()
                        || persisted.draft_text.trim() == prompt.text.trim())
            });
            loaded.goal = persisted.goal.clone();
            loaded.restored = Some((persisted.total_cost_usd, draft));
            restore_conversation(&mut loaded.conversation, persisted);
            if recover_images {
                loaded.attachments = unsent_attachments(&loaded.conversation);
            }
        }
        Ok(None) => {}
        result => {
            loaded.failure = Some(match result {
                Err(error) => error.to_string(),
                _ => "the transcript belongs to a different thread".to_string(),
            });
            loaded.conversation.set_error("The saved conversation could not be read. Its files have been preserved; start a new thread to continue.");
        }
    }
    loaded
}

pub(crate) fn restore_conversation(
    conversation: &mut Conversation,
    persisted: PersistedConversation,
) {
    conversation.replace_full_history(persisted.messages);
    if let Some(session_id) = persisted.session_id {
        conversation.set_session_id(session_id);
    }
    conversation.restore_usage(fx_agent::conversation::UsageTotals {
        input_tokens: persisted.total_input_tokens,
        output_tokens: persisted.total_output_tokens,
        cached_input_tokens: persisted.total_cached_input_tokens,
        reasoning_output_tokens: persisted.total_reasoning_output_tokens,
        total_tokens: persisted.total_tokens,
        current_context_tokens: persisted.current_context_tokens,
        reported_context_window: persisted.reported_context_window,
    });
    if let Some(prompt) = persisted.unsent_prompt {
        conversation.hold_unsent(fx_agent::conversation::UnsentPrompt {
            text: prompt.text,
            attachment_count: prompt.attachment_count,
            session_id: prompt.session_id,
        });
    }
}

fn visible_agent(record: &fx_store::PersistedProject, preferred: Option<Uuid>) -> Option<Uuid> {
    let visible = |id: &Uuid| {
        !record.archived_agent_ids.contains(id)
            && !record.missing_native_agent_ids.contains(id)
            && record.agents.iter().any(|agent| agent.id == *id)
    };
    preferred
        .filter(visible)
        .or_else(|| {
            record
                .project
                .as_ref()
                .and_then(|project| project.agent_order.iter().copied().find(visible))
        })
        .or_else(|| record.agents.iter().map(|agent| agent.id).find(visible))
}

fn interaction_id(event: &fx_agent::ProviderEvent) -> Option<&str> {
    match event {
        fx_agent::ProviderEvent::Approval(request) => Some(&request.id),
        fx_agent::ProviderEvent::UserInput(request) => Some(&request.id),
        _ => None,
    }
}

/// The design settings a fresh install starts with.
pub fn default_theme() -> Theme {
    Theme::new(
        AppearanceMode::Dark,
        BaseTone::Zinc,
        AccentColor::Violet,
        TextSizePreset::Standard,
    )
}

/// Open the add-project sheet above the whole shell.
///
/// A free function rather than a method: the sheet's callback needs the
/// `Entity<AppState>` to write through, and in GPUI 0.2.2 there is no way to get
/// the current entity from inside `App`.
pub fn open_project_picker(state: Entity<AppState>, window: &mut gpui::Window, cx: &mut App) {
    if crate::workspace_ui::has_modal(&state, cx) {
        return;
    }
    let menu = state.read(cx).menu_layer.clone();
    menu.update(cx, |menu, cx| {
        if menu.dismiss().is_some() {
            cx.notify();
        }
    });
    let for_add = state.clone();
    let for_cancel = state.clone();
    let picker = crate::project_picker::ProjectPicker::open(
        window,
        cx,
        Box::new(
            move |root: String, window: &mut gpui::Window, cx: &mut App| {
                for_add.update(cx, |state, cx| {
                    state.add_project(&root);
                    state.picker = None;
                    cx.notify();
                });
                window.blur();
            },
        ),
        Box::new(move |window: &mut gpui::Window, cx: &mut App| {
            for_cancel.update(cx, |state, cx| {
                state.picker = None;
                cx.notify();
            });
            window.blur();
        }),
    );
    let open_roots = state.read(cx).open_project_roots();
    picker.update(cx, |picker, _| picker.set_open_roots(open_roots));
    state.update(cx, |state, cx| {
        state.picker = Some(picker);
        cx.notify();
    });
}

#[cfg(test)]
mod auto_title_tests {
    use super::*;
    use fx_core::{ConversationMessage, MessageRole};

    fn new_chat(cx: &mut App, paths: AppPaths, root: &std::path::Path) -> (Entity<AppState>, Uuid) {
        let state = cx.new(|cx| AppState::restore(paths, cx));
        let id = state.update(cx, |state, _| {
            state.add_project(root.to_str().unwrap());
            let id = state.create_thread().unwrap();
            let mut conversation = state.conversations[&id].lock().unwrap();
            conversation.append(ConversationMessage::text(
                MessageRole::User,
                "Fix sidebar alignment and resize behavior",
            ));
            conversation.append(ConversationMessage::text(
                MessageRole::Assistant,
                "Aligned the rows and added resizing.",
            ));
            id
        });
        (state, id)
    }

    fn pending_job(state: &mut AppState, id: Uuid) -> Uuid {
        let request = state.prepare_auto_title(id).unwrap();
        let generation = Uuid::new_v4();
        state.title_jobs.insert(
            id,
            crate::auto_title::TitleJob {
                generation,
                provider: request.provider,
                model: request.model,
            },
        );
        generation
    }

    #[test]
    fn auto_title_uses_one_separate_model_request_and_preserves_history_and_native_override() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, id) = test.update(|cx| new_chat(cx, paths.clone(), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.preferences.auto_titles.model_id = "lightweight-title-model".into();
                state.show_prompt_title(id);
                assert_eq!(
                    state.thread(id).unwrap().title,
                    "Fix sidebar alignment and resize behavior"
                );
                let request = state.prepare_auto_title(id).unwrap();
                assert_eq!(request.provider, "codex");
                assert_eq!(request.model, "lightweight-title-model");
                assert_eq!(request.user, "Fix sidebar alignment and resize behavior");
                assert!(state.prepare_auto_title(id).is_none());
                assert!(
                    state.store.load_app().unwrap().projects[0]
                        .auto_title_pending
                        .is_empty()
                );
                let generation = Uuid::new_v4();
                state.title_jobs.insert(
                    id,
                    crate::auto_title::TitleJob {
                        generation,
                        provider: "codex".into(),
                        model: "lightweight-title-model".into(),
                    },
                );
                // A later foreground turn must not make the background title call
                // trip the manual rename's busy-thread guard.
                state.conversations[&id]
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::Phase::Running);
                state.finish_auto_title(id, generation, Ok("Improve sidebar layout".into()));
                assert_eq!(state.thread(id).unwrap().title, "Improve sidebar layout");
                assert!(state.last_error.is_none());
                assert_eq!(state.conversations[&id].lock().unwrap().messages().len(), 2);
                assert_eq!(
                    state.snapshot.projects[0].thread_overrides[0]
                        .title
                        .as_deref(),
                    Some("Improve sidebar layout")
                );
                assert!(!state.has_title_jobs());
            })
        });
        let restored = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            restored.update(cx, |state, _| {
                assert_eq!(state.thread(id).unwrap().title, "Improve sidebar layout");
                assert!(state.prepare_auto_title(id).is_none());
            })
        });
    }

    #[test]
    fn imported_manual_archived_deleted_and_opted_out_chats_reject_automatic_names() {
        for action in [
            "manual-before",
            "manual-during",
            "archive",
            "delete",
            "off",
            "naming-provider-change",
            "naming-model-change",
            "imported",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let test = gpui::TestAppContext::single();
            let (state, id) = test
                .update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
            test.update(|cx| {
                state.update(cx, |state, _| {
                    if action == "imported" {
                        state.snapshot.projects[0].auto_title_pending.clear();
                        assert!(state.prepare_auto_title(id).is_none());
                        return;
                    }
                    if action == "manual-before" {
                        assert!(state.rename_thread(id, "My chosen name"));
                        assert!(state.prepare_auto_title(id).is_none());
                        return;
                    }
                    let generation = pending_job(state, id);
                    match action {
                        "manual-during" => assert!(state.rename_thread(id, "My chosen name")),
                        "archive" => assert!(state.set_thread_archived(id, true)),
                        "delete" => assert!(state.delete_thread(id)),
                        "off" => state.preferences.auto_titles.enabled = false,
                        "naming-provider-change" => {
                            state.preferences.auto_titles.provider_id = "claude".into();
                        }
                        "naming-model-change" => {
                            state.preferences.auto_titles.model_id = "another-title-model".into();
                        }
                        _ => unreachable!(),
                    }
                    state.finish_auto_title(id, generation, Ok("Stale generated name".into()));
                    assert!(!state.has_title_jobs());
                    assert!(
                        state
                            .thread(id)
                            .is_none_or(|agent| agent.title != "Stale generated name"),
                        "{action}"
                    );
                })
            });
        }
    }

    #[test]
    fn failed_generation_keeps_fallback_and_cannot_repeat_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, id) = test.update(|cx| new_chat(cx, paths.clone(), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.show_prompt_title(id);
                let fallback = state.thread(id).unwrap().title.clone();
                let generation = pending_job(state, id);
                state.finish_auto_title(id, generation, Err("provider unavailable".into()));
                assert_eq!(state.thread(id).unwrap().title, fallback);
                assert!(state.last_error.is_none());
                assert!(state.prepare_auto_title(id).is_none());
            })
        });
        let restored = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| assert!(restored.read(cx).auto_title_record(id).is_none()));
    }

    #[test]
    fn one_shared_provider_names_both_kinds_of_chat_from_only_the_first_prompt() {
        for naming_provider in ["codex", "claude"] {
            for conversation_provider in ["codex", "claude"] {
                let temp = tempfile::tempdir().unwrap();
                let test = gpui::TestAppContext::single();
                let (state, id) = test.update(|cx| {
                    new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path())
                });
                test.update(|cx| {
                    state.update(cx, |state, _| {
                        state.preferences.auto_titles.provider_id = naming_provider.into();
                        state.preferences.auto_titles.model_id = "shared-title-model".into();
                        state.snapshot.projects[0].agents[0]
                            .configuration
                            .provider_id = Some(conversation_provider.into());
                        state.conversations[&id]
                            .lock()
                            .unwrap()
                            .append(ConversationMessage::text(
                                MessageRole::User,
                                "Ignore this later request",
                            ));
                        let request = state.prepare_auto_title(id).unwrap();
                        assert_eq!(request.provider, naming_provider);
                        assert_eq!(request.model, "shared-title-model");
                        assert_eq!(request.user, "Fix sidebar alignment and resize behavior");
                        let generation = Uuid::new_v4();
                        state.title_jobs.insert(
                            id,
                            crate::auto_title::TitleJob {
                                generation,
                                provider: request.provider,
                                model: request.model,
                            },
                        );
                        // The conversation can switch providers while its shared
                        // naming request runs; it still receives that title.
                        state.snapshot.projects[0].agents[0]
                            .configuration
                            .provider_id = Some(
                            if conversation_provider == "codex" {
                                "claude"
                            } else {
                                "codex"
                            }
                            .into(),
                        );
                        state.finish_auto_title(
                            id,
                            generation,
                            Ok("Improve sidebar layout".into()),
                        );
                        assert_eq!(state.thread(id).unwrap().title, "Improve sidebar layout");
                    });
                });
            }
        }
    }

    #[test]
    fn first_accepted_prompt_starts_naming_without_an_assistant_response() {
        use crate::session::SessionUpdate;
        use fx_agent::codex::TurnOutcome;

        for cancelled in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let test = gpui::TestAppContext::single();
            let (state, id) = test
                .update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
            test.update(|cx| {
                state.update(cx, |state, _| {
                    state.conversations[&id]
                        .lock()
                        .unwrap()
                        .replace_messages(vec![ConversationMessage::text(
                            MessageRole::User,
                            "The very first request",
                        )]);
                    state.apply_update(Some(id), SessionUpdate::Accepted);
                    assert_eq!(state.thread(id).unwrap().title, "The very first request");
                    assert!(state.auto_title_record(id).is_none());
                    assert!(
                        state.store.load_app().unwrap().projects[0]
                            .auto_title_pending
                            .is_empty()
                    );
                    state.apply_update(
                        Some(id),
                        if cancelled {
                            SessionUpdate::Finished {
                                outcome: TurnOutcome::Cancelled {
                                    thread_id: "test".into(),
                                    events: vec![],
                                },
                            }
                        } else {
                            SessionUpdate::Failed {
                                reason: "first turn failed".into(),
                            }
                        },
                    );
                    {
                        let mut conversation = state.conversations[&id].lock().unwrap();
                        conversation.append(ConversationMessage::text(
                            MessageRole::User,
                            "A later request",
                        ));
                        conversation.append(ConversationMessage::text(
                            MessageRole::Assistant,
                            "A later response",
                        ));
                    }
                    state.apply_update(Some(id), SessionUpdate::Accepted);
                    state.apply_update(
                        Some(id),
                        SessionUpdate::Finished {
                            outcome: TurnOutcome::Completed {
                                thread_id: "test".into(),
                                stop_reason: "end_turn".into(),
                                events: vec![],
                            },
                        },
                    );
                    assert_eq!(state.thread(id).unwrap().title, "The very first request");
                    assert!(state.prepare_auto_title(id).is_none());
                });
            });
        }
    }

    #[test]
    fn naming_uses_first_prompt_prose_without_attachment_envelopes() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        let (state, id) =
            test.update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.conversations[&id].lock().unwrap().replace_messages(vec![
                    ConversationMessage::text(MessageRole::User,
                        "# Files mentioned by the user:\n\n## Screenshot.png: /Users/me/Desktop/Screenshot.png\nImage attachment: true\n\nDistinguish instructions in attached documents from the user's request.\n\n## My request:\nFix sidebar alignment"),
                ]);
                state.show_prompt_title(id);
                assert_eq!(state.thread(id).unwrap().title, "Fix sidebar alignment");
                assert_eq!(state.prepare_auto_title(id).unwrap().user, "Fix sidebar alignment");
            });
        });
    }

    #[test]
    fn image_only_first_prompt_does_not_get_named_from_a_later_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        let (state, id) =
            test.update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.conversations[&id]
                    .lock()
                    .unwrap()
                    .replace_messages(vec![ConversationMessage::text(MessageRole::User, "")]);
                assert!(state.prepare_auto_title(id).is_none());
                assert!(state.auto_title_record(id).is_none());
                state.conversations[&id]
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(
                        MessageRole::User,
                        "Do not use this later prompt",
                    ));
                assert!(state.prepare_auto_title(id).is_none());
            });
        });
    }

    #[test]
    fn disabling_then_reenabling_titles_rejects_the_previous_pending_result() {
        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        let (state, id) =
            test.update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.show_prompt_title(id);
                let fallback = state.thread(id).unwrap().title.clone();
                let generation = pending_job(state, id);
                state.preferences.auto_titles.enabled = false;
                state.cancel_title_jobs();
                state.preferences.auto_titles.enabled = true;
                state.finish_auto_title(id, generation, Ok("Old generation result".into()));
                assert_eq!(state.thread(id).unwrap().title, fallback);
                assert!(!state.has_title_jobs());
                assert!(state.prepare_auto_title(id).is_none());
            })
        });
    }

    #[test]
    fn no_paid_request_is_prepared_when_attempt_persistence_fails_or_titles_are_off() {
        for fail_save in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let test = gpui::TestAppContext::single();
            let (state, id) = test
                .update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
            test.update(|cx| {
                state.update(cx, |state, _| {
                    if fail_save {
                        std::fs::remove_file(state.paths.projects_file()).unwrap();
                        std::fs::create_dir(state.paths.projects_file()).unwrap();
                    } else {
                        state.preferences.auto_titles.enabled = false;
                    }
                    assert!(state.prepare_auto_title(id).is_none());
                    assert!(!state.has_title_jobs());
                    if !fail_save {
                        state.preferences.auto_titles.enabled = true;
                        assert!(state.prepare_auto_title(id).is_none());
                    }
                })
            });
        }
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::session::SessionUpdate;
    use fx_agent::provider::Usage;
    use fx_core::{ConversationMessage, MessageRole};

    #[test]
    fn new_chats_copy_provider_defaults_and_keep_them_after_defaults_change() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let mut preferences = PersistedPreferences {
            codex_defaults: fx_store::ProviderDefaults {
                model_id: Some("gpt-6-astra".into()),
                effort: Some("high".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
            },
            ..Default::default()
        };
        Store::new(paths.clone())
            .save_preferences(&preferences)
            .unwrap();
        let test = gpui::TestAppContext::single();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        let first = test.update(|cx| {
            state.update(cx, |state, _| {
                assert_eq!(state.preferences, preferences);
                state.add_project(temp.path().to_str().unwrap());
                let first = state.create_thread().unwrap();
                let configuration = &state.thread(first).unwrap().configuration;
                assert_eq!(configuration.provider_id.as_deref(), Some("codex"));
                assert_eq!(configuration.model_id.as_deref(), Some("gpt-6-astra"));
                assert_eq!(configuration.effort.as_deref(), Some("high"));
                assert_eq!(
                    configuration.agent_access,
                    Some(fx_core::AgentAccess::Supervised)
                );
                assert_eq!(configuration.context_window_size, Some(272_000));
                state.save();
                first
            })
        });
        preferences.codex_defaults.effort = Some("low".into());
        preferences.codex_defaults.agent_access = Some(fx_core::AgentAccess::AcceptEdits);
        Store::new(paths.clone())
            .save_preferences(&preferences)
            .unwrap();
        let restarted = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            restarted.update(cx, |state, _| {
                assert_eq!(
                    state.thread(first).unwrap().configuration.effort.as_deref(),
                    Some("high")
                );
                assert_eq!(
                    state.thread(first).unwrap().configuration.agent_access,
                    Some(fx_core::AgentAccess::Supervised)
                );
                let second = state.create_thread().unwrap();
                assert_eq!(
                    state
                        .thread(second)
                        .unwrap()
                        .configuration
                        .effort
                        .as_deref(),
                    Some("low")
                );
                assert_eq!(
                    state.thread(second).unwrap().configuration.agent_access,
                    Some(fx_core::AgentAccess::AcceptEdits)
                );
            })
        });
    }

    #[test]
    fn new_chat_uses_native_provider_without_copying_or_changing_native_settings() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, imported, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.preferences.claude_defaults = fx_store::ProviderDefaults {
                    model_id: Some("custom-claude-model".into()),
                    effort: Some("low".into()),
                    agent_access: Some(fx_core::AgentAccess::AcceptEdits),
                };
                let native: fx_store::PersistedBinding =
                    serde_json::from_value(serde_json::json!({
                        "agentId": imported,
                        "providerId": "claude",
                        "sessionId": "native-session",
                        "updatedAt": fx_core::models::now(),
                        "model": "native-model",
                        "effort": "high",
                        "agentAccess": "supervised"
                    }))
                    .unwrap();
                state.snapshot.projects[0].native_bindings.push(native);
                let original = state.thread(imported).unwrap().configuration.clone();
                let created = state.create_thread().unwrap();
                let configuration = &state.thread(created).unwrap().configuration;
                assert_eq!(configuration.provider_id.as_deref(), Some("claude"));
                assert_eq!(
                    configuration.model_id.as_deref(),
                    Some("custom-claude-model")
                );
                assert_eq!(configuration.effort.as_deref(), Some("low"));
                assert_eq!(
                    configuration.agent_access,
                    Some(fx_core::AgentAccess::AcceptEdits)
                );
                assert_eq!(state.thread(imported).unwrap().configuration, original);
                let effective = crate::configuration::with_native_defaults(
                    state.thread(imported).unwrap().clone(),
                    state.native_defaults(imported),
                );
                assert_eq!(
                    effective.configuration.model_id.as_deref(),
                    Some("native-model")
                );
                assert_eq!(effective.configuration.effort.as_deref(), Some("high"));
                assert_eq!(
                    effective.configuration.agent_access,
                    Some(fx_core::AgentAccess::Supervised)
                );
                // The next new chat follows the explicit provider on the previous new chat.
                let next = state.create_thread().unwrap();
                assert_eq!(
                    state.thread(next).unwrap().configuration,
                    state.thread(created).unwrap().configuration
                );
            })
        });
    }

    #[test]
    fn provider_defaults_preserve_unknown_efforts_and_clear_known_unsupported_values() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.preferences.codex_defaults = fx_store::ProviderDefaults {
                    model_id: Some("gpt-6-astra".into()),
                    effort: Some("unsupported-effort".into()),
                    ..Default::default()
                };
                let codex = state.default_configuration("codex");
                assert_eq!(codex.context_window_size, Some(272_000));
                assert!(codex.effort.is_none());
                state.preferences.claude_defaults.effort = Some("high".into());
                let claude = state.default_configuration("claude");
                assert_eq!(claude.effort.as_deref(), Some("high"));
                assert!(claude.context_window_size.is_none());
                assert_eq!(
                    crate::provider_catalog::effective_effort(
                        "claude",
                        "default",
                        claude.effort.as_deref()
                    )
                    .as_deref(),
                    Some("high")
                );
                state.preferences.claude_defaults.model_id = Some("unknown-custom-model".into());
                assert_eq!(
                    state.default_configuration("claude").effort.as_deref(),
                    Some("high")
                );
            })
        });
    }

    #[test]
    fn a_resized_sidebar_retains_its_width_through_hide_and_restart() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let state = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.set_sidebar_width(320.5);
                state.toggle_sidebar();
                state.save();
            });
        });
        let restored = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            restored.update(cx, |state, _| {
                assert!(!state.sidebar_visible);
                assert_eq!(state.sidebar_width(), 320.5);
                state.toggle_sidebar();
                assert_eq!(state.sidebar_width(), 320.5);
                state.set_sidebar_width(10.0);
                assert_eq!(state.sidebar_width(), Layout::SIDEBAR_MIN_WIDTH);
                state.set_sidebar_width(9000.0);
                assert_eq!(state.sidebar_width(), Layout::SIDEBAR_MAX_WIDTH);
                state.set_sidebar_width(f32::NAN);
                assert_eq!(state.sidebar_width(), Layout::SIDEBAR_WIDTH);
            });
        });
    }

    fn workspace(cx: &mut App, paths: &AppPaths) -> (Entity<AppState>, Uuid, Uuid, Uuid) {
        let mut project = Project::new(paths.root().parent().unwrap().to_string_lossy());
        let first = Agent::new("First");
        let second = Agent::new("Second");
        project.agent_order = vec![first.id, second.id];
        let project_id = project.id;
        let snapshot = PersistedApp {
            active_project_id: Some(project.id),
            active_agent_id: Some(first.id),
            projects: vec![fx_store::PersistedProject {
                project: Some(project),
                agents: vec![first.clone(), second.clone()],
                ..Default::default()
            }],
            ..Default::default()
        };
        Store::new(paths.clone()).save_app(&snapshot).unwrap();
        (
            cx.new(|cx| AppState::restore(paths.clone(), cx)),
            project_id,
            first.id,
            second.id,
        )
    }

    #[test]
    fn archiving_survives_restart_and_preserves_history_drafts_and_session() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let conversation = state.conversation(project, first);
                {
                    let mut conversation = conversation.lock().unwrap();
                    conversation
                        .append(ConversationMessage::text(MessageRole::User, "Keep history"));
                    conversation.set_session_id("native-session");
                }
                let draft = state.draft(first, cx);
                draft.update(cx, |field, cx| field.set_value("Keep draft", cx));
                state.save_all(cx);
                let before = std::fs::read(paths.conversation_file(project, first)).unwrap();
                assert!(state.set_thread_archived(first, true));
                assert_eq!(state.snapshot.active_agent_id, Some(second));
                assert_eq!(
                    state
                        .active_agents()
                        .iter()
                        .map(|agent| agent.id)
                        .collect::<Vec<_>>(),
                    vec![second]
                );
                assert_eq!(
                    std::fs::read(paths.conversation_file(project, first)).unwrap(),
                    before
                );
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                assert!(state.is_thread_archived(first));
                assert_eq!(state.snapshot.active_agent_id, Some(second));
                state.select_agent(first);
                assert_eq!(
                    state.active_agent().unwrap().id,
                    first,
                    "explicit selection can read archived history"
                );
                let conversation = state.conversation(project, first);
                assert_eq!(
                    conversation.lock().unwrap().session_id(),
                    Some("native-session")
                );
                assert_eq!(
                    conversation
                        .lock()
                        .unwrap()
                        .messages()
                        .next()
                        .unwrap()
                        .text_content(),
                    "Keep history"
                );
                assert_eq!(state.draft(first, cx).read(cx).value(), "Keep draft");
                state.select_project(project);
                assert_eq!(state.snapshot.active_agent_id, Some(second));
                assert!(state.set_thread_archived(first, false));
                assert_eq!(
                    state.snapshot.active_agent_id,
                    Some(second),
                    "restoring does not force selection"
                );
                assert_eq!(state.active_agents().len(), 2);
                assert!(state.set_thread_archived(first, true));
                assert!(state.set_thread_archived(second, true));
                assert_eq!(state.snapshot.active_agent_id, None);
                state.select_agent(first);
                state.save();
            })
        });
        test.update(|cx| {
            let reopened = cx.new(|cx| AppState::restore(paths.clone(), cx));
            assert_eq!(
                reopened.read(cx).snapshot.active_agent_id,
                None,
                "restart never automatically selects an archive"
            );
        });
    }

    #[test]
    fn rename_targets_an_inactive_thread_and_persists_without_moving_selection() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                assert!(state.rename_thread(second, "  Renamed thread  "));
                assert_eq!(state.thread(second).unwrap().title, "Renamed thread");
                assert_eq!(state.thread(first).unwrap().title, "First");
                assert_eq!(state.snapshot.active_agent_id, Some(first));
                assert!(!state.rename_thread(second, "  \n "));
                assert_eq!(state.thread(second).unwrap().title, "Renamed thread");
                assert!(state.set_thread_archived(second, true));
                assert_eq!(state.snapshot.active_agent_id, Some(first));
            })
        });
        let saved = Store::new(paths).load_app().unwrap();
        assert_eq!(saved.projects[0].agents[1].title, "Renamed thread");
        assert_eq!(saved.projects[0].archived_agent_ids, vec![second]);
    }

    #[test]
    fn deleting_an_inactive_thread_removes_only_its_local_data_and_ignores_late_updates() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let source = temp.path().join("source.txt");
        std::fs::write(&source, "project content").unwrap();
        let test = gpui::TestAppContext::single();
        let (state, project, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                for id in [first, second] {
                    state
                        .conversation(project, id)
                        .lock()
                        .unwrap()
                        .append(ConversationMessage::text(MessageRole::User, id.to_string()));
                    state
                        .draft(id, cx)
                        .update(cx, |field, cx| field.set_value("draft", cx));
                }
                let record = &mut state.snapshot.projects[0];
                record.workspaces.push(fx_store::PersistedWorkspace {
                    agent_id: second,
                    ..Default::default()
                });
                record.native_bindings.push(fx_store::PersistedBinding {
                    agent_id: second,
                    provider_id: "codex".into(),
                    session_id: "native-session".into(),
                    title: "Second".into(),
                    working_directory: temp.path().to_string_lossy().into_owned(),
                    updated_at: fx_core::models::now(),
                    model: None,
                    effort: None,
                    agent_access: None,
                    is_archived: false,
                    revision: String::new(),
                    context_window_size: None,
                    local_history_pending: false,
                });
                record
                    .native_presentation_ids
                    .push(fx_store::format::PersistedPresentation {
                        agent_id: second,
                        provider_id: "codex".into(),
                        session_id: "native-session".into(),
                    });
                state.save_all(cx);
                let first_path = paths.conversation_file(project, first);
                let first_bytes = std::fs::read(&first_path).unwrap();
                let first_backup =
                    std::fs::read(fx_store::write::backup_path(&first_path)).unwrap();
                let second_path = paths.conversation_file(project, second);
                let assets = paths.assets_dir(project, second);
                std::fs::create_dir_all(&assets).unwrap();
                std::fs::write(assets.join("attachment.png"), "cached image").unwrap();
                state.protected_conversations.insert(second);
                state.retry_drafts.insert(second, "retry this".into());
                state.restored_costs.insert(second, 2.0);
                let request =
                    fx_agent::ProviderEvent::UserInput(fx_agent::provider::UserInputRequest {
                        id: "old-request".into(),
                        questions: vec![],
                    });
                state
                    .pending_interactions
                    .entry(second)
                    .or_default()
                    .push_back(request.clone());
                let late = state.update_sender_for(second);
                assert!(state.delete_thread(second));
                assert_eq!(state.snapshot.active_agent_id, Some(first));
                assert!(state.thread(second).is_none());
                assert!(!state.conversations.contains_key(&second));
                assert!(!state.drafts.contains_key(&second));
                assert!(!state.protected_conversations.contains(&second));
                assert!(!state.restored_drafts.contains_key(&second));
                assert!(!state.retry_drafts.contains_key(&second));
                assert!(!state.saved_conversations.contains_key(&second));
                assert!(!state.restored_costs.contains_key(&second));
                assert!(state.next_question().is_none());
                let record = &state.snapshot.projects[0];
                assert!(record.workspaces.is_empty());
                assert!(record.native_bindings.is_empty());
                assert!(record.native_presentation_ids.is_empty());
                assert_eq!(record.project.as_ref().unwrap().agent_order, vec![first]);
                assert!(!second_path.exists());
                assert!(!fx_store::write::backup_path(&second_path).exists());
                assert!(!assets.exists());
                late(SessionUpdate::Interaction { event: request });
                late(SessionUpdate::Failed {
                    reason: "late failure".into(),
                });
                state.drain_updates();
                assert!(state.next_question().is_none());
                assert!(state.last_error.is_none());
                state.save_all(cx);
                assert!(
                    !second_path.exists(),
                    "autosave must not resurrect deleted history"
                );
                assert_eq!(std::fs::read(first_path).unwrap(), first_bytes);
                assert_eq!(
                    std::fs::read(fx_store::write::backup_path(
                        &paths.conversation_file(project, first)
                    ))
                    .unwrap(),
                    first_backup
                );
                assert!(state.delete_thread(first));
                assert_eq!(state.snapshot.active_agent_id, None);
            })
        });
        assert_eq!(std::fs::read_to_string(source).unwrap(), "project content");
        assert!(
            Store::new(paths).load_app().unwrap().projects[0]
                .agents
                .is_empty()
        );
    }

    #[test]
    fn running_threads_refuse_all_lifecycle_changes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let conversation = state.conversation(project, first);
                for phase in [
                    fx_agent::conversation::Phase::Running,
                    fx_agent::conversation::Phase::WaitingForApproval,
                ] {
                    conversation.lock().unwrap().set_phase(phase);
                    assert!(!state.rename_thread(first, "Must not change"));
                    assert!(!state.set_thread_archived(first, true));
                    assert!(!state.delete_thread(first));
                    assert_eq!(state.thread(first).unwrap().title, "First");
                    assert!(!state.is_thread_archived(first));
                    assert!(
                        state
                            .last_error
                            .as_deref()
                            .unwrap()
                            .contains("running turn")
                    );
                }
            })
        });
    }

    #[test]
    fn failed_lifecycle_saves_preserve_the_live_thread_and_history() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state
                    .conversation(project, first)
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::User, "preserve"));
                state.save_all(cx);
                let before = serde_json::to_value(&state.snapshot).unwrap();
                let history = std::fs::read(paths.conversation_file(project, first)).unwrap();
                std::fs::remove_file(paths.projects_file()).unwrap();
                std::fs::create_dir(paths.projects_file()).unwrap();
                assert!(!state.rename_thread(first, "Changed"));
                assert!(!state.set_thread_archived(first, true));
                assert!(!state.delete_thread(first));
                assert_eq!(serde_json::to_value(&state.snapshot).unwrap(), before);
                assert_eq!(
                    std::fs::read(paths.conversation_file(project, first)).unwrap(),
                    history
                );
                assert!(state.last_error.is_some());
            })
        });
    }

    #[test]
    fn failed_file_cleanup_keeps_a_committed_deletion_and_reports_the_remaining_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, second) = test.update(|cx| workspace(cx, &paths));
        let blocked = paths.conversation_file(project, first);
        std::fs::create_dir_all(&blocked).unwrap();
        test.update(|cx| {
            state.update(cx, |state, _| {
                assert!(
                    state.delete_thread(first),
                    "sidebar deletion has already committed"
                );
                assert!(state.thread(first).is_none());
                assert_eq!(state.snapshot.active_agent_id, Some(second));
                assert!(
                    state
                        .last_error
                        .as_deref()
                        .unwrap()
                        .contains("saved files could not all be deleted")
                );
            })
        });
        assert!(blocked.exists());
        let saved = Store::new(paths).load_app().unwrap();
        assert_eq!(saved.projects[0].agents.len(), 1);
        assert_eq!(saved.projects[0].agents[0].id, second);
    }

    #[test]
    fn sending_to_an_archive_requires_a_successfully_persisted_restore() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.set_thread_archived(first, true));
                state.select_agent(first);
                assert!(state.prepare_prompt(first, "Resume this conversation", cx));
                assert!(!state.is_thread_archived(first));
                assert!(state.set_thread_archived(first, true));
                std::fs::remove_file(paths.projects_file()).unwrap();
                std::fs::create_dir(paths.projects_file()).unwrap();
                assert!(!state.prepare_prompt(first, "Do not send", cx));
                assert!(state.is_thread_archived(first));
                assert_eq!(
                    state.conversations[&first].lock().unwrap().message_count(),
                    1
                );
            })
        });
    }

    #[test]
    fn accepted_turn_and_usage_survive_a_real_store_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("remember this", cx));
                assert!(state.prepare_prompt(agent, "remember this", cx));
                assert!(
                    field.read(cx).value().is_empty(),
                    "clear before any provider event"
                );
                // The save is queued on the writer; wait for it before reading.
                state.flush_saves();
                let saved = state
                    .store
                    .load_conversation(project, agent)
                    .unwrap()
                    .unwrap();
                assert!(saved.draft_text.is_empty());
                assert_eq!(saved.unsent_prompt.unwrap().text, "remember this");
                let conversation = state.conversation(project, agent);
                let update = state.update_sender_for(agent);
                update(SessionUpdate::Started {
                    thread_id: "provider-thread-123".into(),
                });
                update(SessionUpdate::Accepted);
                {
                    let mut conversation = conversation.lock().unwrap();
                    conversation.record_usage(Usage {
                        input_tokens: 10,
                        output_tokens: 20,
                        cached_input_tokens: 4,
                        reasoning_output_tokens: 5,
                        total_tokens: 30,
                    });
                    conversation.record_context(100, 1000);
                    conversation.append_streaming("I will remember.");
                    conversation.commit_streaming();
                }
                state.drain_updates_with_context(cx);
                assert!(field.read(cx).value().is_empty());
                state.save_all(cx);
            });
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                let conversation = state.conversation(project, agent);
                let conversation = conversation.lock().unwrap();
                assert_eq!(conversation.session_id(), Some("provider-thread-123"));
                assert_eq!(conversation.message_count(), 2);
                assert_eq!(
                    conversation.last_text().as_deref(),
                    Some("I will remember.")
                );
                assert_eq!(conversation.usage().input_tokens, 10);
                assert_eq!(conversation.usage().total_tokens, 30);
                assert_eq!(conversation.usage().current_context_tokens, 100);
                assert_eq!(conversation.usage().reported_context_window, Some(1000));
                assert!(conversation.unsent().is_none());
                drop(conversation);
                assert!(state.draft(agent, cx).read(cx).value().is_empty());
            })
        });
    }

    #[test]
    fn a_failed_launch_restores_both_the_prompt_and_the_composer() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("do not lose this 🜁", cx));
                let prompt = field.read(cx).value().to_string();
                assert!(state.prepare_prompt(agent, &prompt, cx));
                assert!(field.read(cx).value().is_empty());
                let update = state.update_sender_for(agent);
                update(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), prompt);
                state.save_all(cx);
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                let conversation = state.conversation(project, agent);
                assert_eq!(
                    conversation.lock().unwrap().unsent().unwrap().text,
                    "do not lose this 🜁"
                );
                assert_eq!(
                    state.draft(agent, cx).read(cx).value(),
                    "do not lose this 🜁"
                );
            })
        });
    }

    #[test]
    fn a_whitespace_only_composer_does_not_stop_a_failed_prompt_coming_back() {
        // `prepare_prompt` trims, so a field holding only a newline is empty
        // as far as sending goes. It must be empty for recovery too.
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("send this", cx));
                assert!(state.prepare_prompt(agent, "send this", cx));
                field.update(cx, |field, cx| field.set_value("\n", cx));
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), "send this");
                state.flush_saves();
                let saved = state
                    .store
                    .load_conversation(project, agent)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    saved.draft_text, "send this",
                    "the failure's own save must not record the newline over the retry"
                );
            })
        });
    }

    #[test]
    fn a_whitespace_only_saved_draft_falls_back_to_the_unsent_prompt_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("send this", cx));
                assert!(state.prepare_prompt(agent, "send this", cx));
                field.update(cx, |field, cx| field.set_value(" \n", cx));
                state.save_all(cx);
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                state.conversation(project, agent);
                assert_eq!(state.draft(agent, cx).read(cx).value(), "send this");
            })
        });

        // A thread whose field was never created recovers through the same
        // rule, from the restored copy rather than from a live field.
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                state.conversation(project, agent);
                state.restored_drafts.insert(agent, "\n".into());
                assert!(state.prepare_prompt(agent, "send this", cx));
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(state.draft(agent, cx).read(cx).value(), "send this");
            })
        });
    }

    fn image(fill: u8) -> fx_core::Attachment {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(48, fill);
        fx_core::Attachment::new(data, "image/png", format!("image-{fill}.png"))
    }

    fn attach(state: &mut AppState, agent: Uuid, images: Vec<fx_core::Attachment>) {
        state.begin_preparing_attachments(agent);
        state.finish_preparing_attachments(
            agent,
            crate::attachments::Prepared {
                attachments: images,
                errors: Vec::new(),
            },
        );
    }

    #[test]
    fn images_go_with_an_unsent_prompt_through_a_retry_and_a_restart() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        let sent = image(1);
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("look at this", cx));
                attach(state, agent, vec![sent.clone()]);
                let images = state.attachments(agent).to_vec();
                assert!(state.prepare_prompt_with(agent, "look at this", &images, cx));
                assert!(state.attachments(agent).is_empty(), "sent images stay out");
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), "look at this");
                assert_eq!(state.attachments(agent).len(), 1, "the image came back");
                assert_eq!(state.attachments(agent)[0].data, sent.data);

                // Retrying the same prompt with the same image reuses its row.
                let images = state.attachments(agent).to_vec();
                assert!(state.prepare_prompt_with(agent, "look at this", &images, cx));
                let conversation = state.conversation(project, agent);
                assert_eq!(conversation.lock().unwrap().message_count(), 1);
                assert_eq!(
                    conversation
                        .lock()
                        .unwrap()
                        .unsent()
                        .unwrap()
                        .attachment_count,
                    1
                );
                state.save_all(cx);
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                state.conversation(project, agent);
                assert_eq!(state.draft(agent, cx).read(cx).value(), "look at this");
                assert_eq!(state.attachments(agent).len(), 1, "lost across a restart");
                assert_eq!(state.attachments(agent)[0].data, sent.data);
            })
        });
    }

    #[test]
    fn a_composer_holding_its_own_images_keeps_them_over_a_failed_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.prepare_prompt_with(agent, "first", &[image(1)], cx));
                attach(state, agent, vec![image(2)]);
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "startup failed".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(state.attachments(agent).len(), 1);
                assert_eq!(state.attachments(agent)[0].data, image(2).data);
            })
        });
    }

    #[test]
    fn the_composer_refuses_images_past_its_limits_and_says_why() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let many = (0..crate::attachments::MAX_PENDING_ATTACHMENTS as u8 + 2)
                    .map(image)
                    .collect();
                attach(state, agent, many);
                assert_eq!(
                    state.attachments(agent).len(),
                    crate::attachments::MAX_PENDING_ATTACHMENTS
                );
                assert!(!state.is_preparing_attachments(agent));
                assert!(state.composer_notice(agent).unwrap().contains("at once"));
                let id = state.attachments(agent)[0].id;
                state.remove_attachment(agent, id);
                assert_eq!(
                    state.attachments(agent).len(),
                    crate::attachments::MAX_PENDING_ATTACHMENTS - 1
                );
            })
        });
    }

    #[test]
    fn a_queued_prompt_keeps_its_images_and_moves_when_the_turn_ends() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.prepare_prompt(agent, "first", cx));
                state.update_sender_for(agent)(SessionUpdate::Accepted);
                state.drain_updates_with_context(cx);

                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("then this", cx));
                attach(state, agent, vec![image(3)]);
                let images = state.attachments(agent).to_vec();
                state
                    .enqueue_prompt(agent, "then this", images, cx)
                    .unwrap();
                assert!(
                    field.read(cx).value().is_empty(),
                    "the queue took the draft"
                );
                assert!(state.attachments(agent).is_empty());
                assert!(
                    state.take_ready_queues().is_empty(),
                    "nothing moves mid-turn"
                );

                state.update_sender_for(agent)(SessionUpdate::Finished {
                    outcome: fx_agent::codex::TurnOutcome::Completed {
                        thread_id: "thread".into(),
                        stop_reason: "completed".into(),
                        events: vec![],
                    },
                });
                state.drain_updates_with_context(cx);
                assert_eq!(state.take_ready_queues(), vec![agent]);
                let (text, images) = state.dequeue_prompt(agent).unwrap();
                assert_eq!(text, "then this");
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].data, image(3).data);
                assert!(state.dequeue_prompt(agent).is_none());
            })
        });
    }

    #[test]
    fn a_queue_does_not_move_behind_a_prompt_the_provider_never_took() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.prepare_prompt(agent, "refused", cx));
                state
                    .enqueue_prompt(agent, "waiting", Vec::new(), cx)
                    .unwrap();
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "writer conflict".into(),
                });
                state.drain_updates_with_context(cx);
                assert!(state.take_ready_queues().is_empty());
            })
        });
    }

    #[test]
    fn a_queued_prompt_is_edited_in_place_and_an_empty_edit_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, agent);
                for text in ["one", "two"] {
                    state.enqueue_prompt(agent, text, Vec::new(), cx).unwrap();
                }
                state.begin_editing_queued(agent, 1, cx);
                let editor = state.queue_editor().unwrap().field.clone();
                assert_eq!(editor.read(cx).value(), "two");
                editor.update(cx, |field, cx| field.set_value("  two, rewritten  ", cx));
                assert!(state.save_queued_edit(cx));
                assert!(state.queue_editor().is_none());
                let conversation = state.conversation(project, agent);
                let queued: Vec<String> = conversation
                    .lock()
                    .unwrap()
                    .queued()
                    .map(|prompt| prompt.text.clone())
                    .collect();
                assert_eq!(queued, vec!["one", "two, rewritten"]);

                state.begin_editing_queued(agent, 0, cx);
                let editor = state.queue_editor().unwrap().field.clone();
                editor.update(cx, |field, cx| field.set_value(" \n ", cx));
                assert!(!state.save_queued_edit(cx));
                assert!(state.queue_editor().is_some(), "the editor stays open");
                state.remove_queued(agent, 0);
                assert!(
                    state.queue_editor().is_none(),
                    "removing it closes its editor"
                );
            })
        });
    }

    #[test]
    fn accepted_guidance_clears_the_composer_and_refused_guidance_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, agent);
                assert!(
                    state.begin_steer(agent, "guide", Vec::new()).is_err(),
                    "there is no turn to steer"
                );
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("guide", cx));
                attach(state, agent, vec![image(4)]);
                let id = Uuid::new_v4();
                state.composer.steers.insert(
                    agent,
                    PendingSteer {
                        id,
                        text: "guide".into(),
                        attachment_ids: vec![state.attachments(agent)[0].id],
                    },
                );
                assert!(state.is_steering(agent));
                // A stale answer for some other steer changes nothing.
                state.update_sender_for(agent)(SessionUpdate::Steered {
                    id: Uuid::new_v4(),
                    result: Ok(()),
                });
                state.drain_updates_with_context(cx);
                assert!(state.is_steering(agent));

                state.update_sender_for(agent)(SessionUpdate::Steered { id, result: Ok(()) });
                state.drain_updates_with_context(cx);
                assert!(!state.is_steering(agent));
                assert!(field.read(cx).value().is_empty());
                assert!(state.attachments(agent).is_empty());

                field.update(cx, |field, cx| field.set_value("again", cx));
                let id = Uuid::new_v4();
                state.composer.steers.insert(
                    agent,
                    PendingSteer {
                        id,
                        text: "again".into(),
                        attachment_ids: Vec::new(),
                    },
                );
                state.update_sender_for(agent)(SessionUpdate::Steered {
                    id,
                    result: Err("the turn ended".into()),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), "again");
                assert!(
                    state
                        .composer_notice(agent)
                        .unwrap()
                        .contains("Your message was not sent")
                );
            })
        });
    }

    #[test]
    fn a_goal_is_kept_with_its_thread_and_a_new_objective_is_sent_after_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        let goal = fx_core::models::ConversationGoal::from_codex(&serde_json::json!({
            "threadId": "thread", "objective": "Ship it", "status": "active"
        }))
        .unwrap();
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, agent);
                state.send_after_goal(agent, "Ship it".into());
                state.update_sender_for(agent)(SessionUpdate::GoalChanged {
                    goal: Some(goal.clone()),
                });
                state.update_sender_for(agent)(SessionUpdate::Finished {
                    outcome: fx_agent::codex::TurnOutcome::Completed {
                        thread_id: "thread".into(),
                        stop_reason: "control".into(),
                        events: vec![],
                    },
                });
                state.drain_updates_with_context(cx);
                assert_eq!(state.goal(agent), Some(&goal));
                assert_eq!(state.take_ready_queues(), vec![agent]);
                assert_eq!(state.dequeue_prompt(agent).unwrap().0, "Ship it");
                state.save_all(cx);
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            reopened.update(cx, |state, cx| {
                state.conversation(project, agent);
                assert_eq!(state.goal(agent), Some(&goal));
                state.update_sender_for(agent)(SessionUpdate::GoalChanged { goal: None });
                state.drain_updates_with_context(cx);
                assert_eq!(state.goal(agent), None);
                state.save_all(cx);
            })
        });
        let reopened = test.update(|cx| cx.new(|cx| AppState::restore(paths, cx)));
        test.update(|cx| {
            reopened.update(cx, |state, _| {
                state.conversation(project, agent);
                assert_eq!(state.goal(agent), None, "a cleared goal came back");
            })
        });
    }

    #[test]
    fn the_follow_up_setting_reads_its_stored_spelling() {
        assert_eq!(FollowUp::from_preference("queue"), FollowUp::Queue);
        assert_eq!(FollowUp::from_preference("steer"), FollowUp::Steer);
        assert_eq!(FollowUp::from_preference(""), FollowUp::Steer);
        assert_eq!(FollowUp::Steer.opposite(), FollowUp::Queue);
        assert_eq!(
            FollowUp::from_preference(FollowUp::Queue.preference()),
            FollowUp::Queue
        );
    }

    #[test]
    fn accepting_a_prompt_does_not_erase_edits_made_during_startup() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("sent prompt", cx));
                assert!(state.prepare_prompt(agent, "sent prompt", cx));
                field.update(cx, |field, cx| field.set_value("next draft", cx));
                state.update_sender_for(agent)(SessionUpdate::Accepted);
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), "next draft");

                // Identical wording can be a deliberate new follow-up. Late
                // acknowledgement must no longer compare and clear its text.
                assert!(state.prepare_prompt(agent, "next draft", cx));
                assert!(field.read(cx).value().is_empty());
                field.update(cx, |field, cx| field.set_value("next draft", cx));
                state.update_sender_for(agent)(SessionUpdate::Accepted);
                state.drain_updates_with_context(cx);
                assert_eq!(field.read(cx).value(), "next draft");
            })
        });
    }

    #[test]
    fn startup_failure_preserves_a_new_draft_and_restores_only_the_originating_thread() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let first_field = state.draft(first, cx);
                first_field.update(cx, |field, cx| field.set_value("send this", cx));
                assert!(state.prepare_prompt(first, "send this", cx));
                first_field.update(cx, |field, cx| field.set_value("my follow-up", cx));
                // Left empty on purpose: a filled composer would also pass if the
                // prompt were restored into every empty one, which is the bug.
                let second_field = state.draft(second, cx);
                state.select_agent(second);
                state.update_sender_for(first)(SessionUpdate::Failed {
                    reason: "startup failed".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(first_field.read(cx).value(), "my follow-up");
                assert_eq!(second_field.read(cx).value(), "");
                state.flush_saves();
                let saved = state
                    .store
                    .load_conversation(project, first)
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.draft_text, "my follow-up");
                assert_eq!(saved.unsent_prompt.unwrap().text, "send this");
                let other = state.store.load_conversation(project, second).unwrap();
                assert!(
                    other.is_none_or(
                        |other| other.draft_text.is_empty() && other.unsent_prompt.is_none()
                    ),
                    "the other chat was given this chat's prompt"
                );

                // With no replacement draft, stopping before acknowledgement makes
                // the original prompt available to retry, even in a background chat.
                first_field.update(cx, |field, cx| field.set_value("send this", cx));
                assert!(state.prepare_prompt(first, "send this", cx));
                state.update_sender_for(first)(SessionUpdate::Finished {
                    outcome: fx_agent::codex::TurnOutcome::Cancelled {
                        thread_id: String::new(),
                        events: vec![],
                    },
                });
                state.drain_updates_with_context(cx);
                assert_eq!(first_field.read(cx).value(), "send this");
                assert_eq!(second_field.read(cx).value(), "");
            })
        });
    }

    #[test]
    fn a_finished_turn_saves_the_outcome_the_sidebar_shows_for_unloaded_threads() {
        use fx_core::AgentExecutionState;

        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.update_sender_for(first)(SessionUpdate::Finished {
                    outcome: fx_agent::codex::TurnOutcome::Completed {
                        thread_id: String::new(),
                        stop_reason: "end_turn".into(),
                        events: vec![],
                    },
                });
                state.update_sender_for(second)(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
                assert_eq!(
                    state.thread(first).unwrap().execution_state,
                    AgentExecutionState::Success
                );
                assert_eq!(
                    state.thread(second).unwrap().execution_state,
                    AgentExecutionState::Failure
                );
            })
        });

        // After a restart nothing is loaded, and the rows still say the same.
        let restarted = test.update(|cx| cx.new(|cx| AppState::restore(paths.clone(), cx)));
        test.update(|cx| {
            let state = restarted.read(cx);
            for (agent, expected) in [
                (first, fx_design::components::ActivityState::Completed),
                (second, fx_design::components::ActivityState::Error),
            ] {
                let saved = state.thread(agent).unwrap().execution_state;
                assert_eq!(
                    crate::thread_presentation::activity(None, false, saved).0,
                    expected
                );
            }
        });
    }

    #[test]
    fn a_failure_after_acceptance_does_not_put_the_sent_prompt_back() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let field = state.draft(agent, cx);
                field.update(cx, |field, cx| field.set_value("sent prompt", cx));
                assert!(state.prepare_prompt(agent, "sent prompt", cx));
                let update = state.update_sender_for(agent);
                update(SessionUpdate::Accepted);
                update(SessionUpdate::Failed {
                    reason: "provider disconnected".into(),
                });
                state.drain_updates_with_context(cx);
                assert!(field.read(cx).value().is_empty());
                assert!(
                    state
                        .conversation_by_agent(agent)
                        .unwrap()
                        .lock()
                        .unwrap()
                        .unsent()
                        .is_none()
                );
            })
        });
    }

    #[test]
    fn a_claude_thread_with_no_known_context_is_checked_once_and_filled_in() {
        // A thread saved before Claude's usage was recorded had no window, so
        // its meter could show nothing until the next turn's result.
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, claude, codex) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.snapshot.projects[0].agents[0]
                    .configuration
                    .provider_id = Some("claude".into());
                state.snapshot.projects[0].agents[1]
                    .configuration
                    .provider_id = Some("codex".into());
                for agent in [claude, codex] {
                    let conversation = state.conversation(project, agent);
                    let mut conversation = conversation.lock().unwrap();
                    conversation.append(fx_core::ConversationMessage::text(
                        fx_core::MessageRole::User,
                        "Earlier",
                    ));
                    conversation.set_session_id("09c18c5c-b6ff-45ca-9603-f5491f51a15d");
                }
                let (session, root) = state.context_probe_target(claude).expect("asked");
                assert_eq!(session, "09c18c5c-b6ff-45ca-9603-f5491f51a15d");
                assert_eq!(root, state.project_root_for(claude).unwrap());
                assert!(state.context_probe_target(codex).is_none(), "only Claude");

                // Once per launch, whatever the answer was.
                state.context_probed.insert(claude);
                assert!(state.context_probe_target(claude).is_none());

                let report = fx_agent::claude::catalog::SessionContext {
                    used: 88_443,
                    window: 1_000_000,
                    model: Some("claude-opus-5-5[1m]".into()),
                };
                // An answer for a session the thread has left is dropped.
                state.apply_probed_context(claude, "another-session", &report);
                assert_eq!(
                    state
                        .conversation(project, claude)
                        .lock()
                        .unwrap()
                        .usage()
                        .reported_context_window,
                    None
                );
                state.apply_probed_context(claude, "09c18c5c-b6ff-45ca-9603-f5491f51a15d", &report);
                let agent = state.thread(claude).unwrap().clone();
                let conversation = state.conversation(project, claude);
                let usage = crate::thread_presentation::context_usage(
                    state,
                    &agent,
                    Some(&conversation.lock().unwrap()),
                );
                assert_eq!(usage.fraction(), Some(88_443.0 / 1_000_000.0));

                // A window a turn reported meanwhile is fresher; keep it.
                conversation.lock().unwrap().record_context(5, 200_000);
                state.apply_probed_context(claude, "09c18c5c-b6ff-45ca-9603-f5491f51a15d", &report);
                assert_eq!(
                    conversation.lock().unwrap().usage().reported_context_window,
                    Some(200_000)
                );
            })
        });
    }

    #[test]
    fn retrying_a_prompt_with_images_resends_them_and_keeps_its_one_row() {
        // The banner's Retry resends the unsent prompt. Its text alone is a
        // different prompt: the images would be lost and a second row added.
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let images = vec![image(1), image(2)];
                assert!(state.prepare_prompt_with(agent, "what is this", &images, cx));
                let conversation = state.conversation(project, agent);
                conversation.lock().unwrap().set_error("launch failed");
                assert_eq!(state.retry_prompt(agent).as_deref(), Some("what is this"));
                let resent = state.unsent_images(agent);
                assert!(same_images(&resent, &images), "the images were not resent");
                assert!(state.prepare_prompt_with(agent, "what is this", &resent, cx));
                let users = conversation
                    .lock()
                    .unwrap()
                    .messages()
                    .filter(|message| message.role == fx_core::MessageRole::User)
                    .count();
                assert_eq!(users, 1, "the retry added a second prompt row");
            })
        });
    }

    #[test]
    fn a_failed_turn_retries_resumes_restarts_and_dismisses() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let conversation = state.conversation(project, agent);
                let fail = |state: &mut AppState, reason: &str| {
                    conversation.lock().unwrap().set_error(reason);
                    state.last_error = Some(reason.into());
                };

                // A prompt the provider never accepted is the one resent.
                assert!(state.prepare_prompt(agent, "first question", cx));
                fail(state, "launch failed");
                assert_eq!(state.retry_prompt(agent).as_deref(), Some("first question"));
                assert!(conversation.lock().unwrap().error().is_none());
                assert!(state.last_error.is_none(), "the shell kept the banner");

                // Resuming needs a session to resume.
                fail(state, "provider disconnected");
                assert_eq!(state.resume_session(agent), None);
                conversation.lock().unwrap().set_session_id("thread-1");
                assert_eq!(state.resume_session(agent).as_deref(), Some("continue"));

                // Restarting forgets the session and its binding, and resends.
                conversation.lock().unwrap().clear_unsent();
                state.bind_local_session(agent);
                fail(state, "Codex thread not found");
                assert_eq!(
                    state.restart_session(agent).as_deref(),
                    Some("first question")
                );
                assert!(conversation.lock().unwrap().session_id().is_none());
                let record = &state.snapshot.projects[0];
                assert!(
                    !record
                        .native_bindings
                        .iter()
                        .any(|binding| binding.agent_id == agent),
                    "the dead session stayed bound"
                );
                assert!(
                    record
                        .suppressed_native_threads
                        .iter()
                        .any(|identity| identity.session_id == "thread-1"),
                    "the dead session could come back as a duplicate thread"
                );
                // The next session the provider opens binds to the same thread.
                state.update_sender_for(agent)(SessionUpdate::Started {
                    thread_id: "thread-2".into(),
                });
                state.drain_updates_with_context(cx);
                assert!(
                    state.snapshot.projects[0]
                        .native_bindings
                        .iter()
                        .any(
                            |binding| binding.agent_id == agent && binding.session_id == "thread-2"
                        )
                );

                // Dismissing leaves an unrelated shell failure up.
                fail(state, "boom");
                state.last_error = Some("The workspace could not be saved.".into());
                state.dismiss_turn_error(agent);
                assert!(conversation.lock().unwrap().error().is_none());
                assert_eq!(
                    state.last_error.as_deref(),
                    Some("The workspace could not be saved.")
                );
            })
        });
    }

    #[test]
    fn a_running_thread_is_never_recovered_underneath_its_turn() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.prepare_prompt(agent, "busy", cx));
                let conversation = state.conversation(project, agent);
                conversation.lock().unwrap().set_session_id("thread-1");
                conversation
                    .lock()
                    .unwrap()
                    .apply(fx_agent::ProviderEvent::Lifecycle(
                        fx_agent::provider::Lifecycle::TurnStarted,
                    ));
                assert_eq!(state.retry_prompt(agent), None);
                assert_eq!(state.resume_session(agent), None);
                assert_eq!(state.restart_session(agent), None);
                assert_eq!(conversation.lock().unwrap().session_id(), Some("thread-1"));
            })
        });
    }

    #[test]
    fn deleting_a_thread_forgets_its_transcript_memory() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, _, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let conversation = state.conversation(project, second);
                let id = conversation.lock().unwrap().id;
                crate::transcript::snapshot(&conversation.lock().unwrap());
                assert!(crate::transcript::remembers(id));
                assert!(state.delete_thread(second));
                assert!(
                    !crate::transcript::remembers(id),
                    "a deleted thread's view memory outlived it"
                );
            })
        });
    }

    #[test]
    fn retrying_an_unaccepted_prompt_does_not_duplicate_its_user_row() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                assert!(state.prepare_prompt(agent, "try again", cx));
                state.update_sender_for(agent)(SessionUpdate::Failed {
                    reason: "launch failed".into(),
                });
                state.drain_updates_with_context(cx);
                assert!(state.prepare_prompt(agent, "try again", cx));
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .message_count(),
                    1
                );
                state.update_sender_for(agent)(SessionUpdate::Accepted);
                state.drain_updates_with_context(cx);
                // Once accepted, the same text is an intentional new message.
                assert!(state.prepare_prompt(agent, "try again", cx));
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .message_count(),
                    2
                );
            })
        });
    }

    #[test]
    fn quitting_saves_drafts_and_partial_output_without_pruning_unopened_threads() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, unopened) = test.update(|cx| workspace(cx, &paths));
        let store = Store::new(paths.clone());
        store
            .save_conversation(
                project,
                unopened,
                &PersistedConversation::from_messages(
                    unopened,
                    vec![ConversationMessage::text(
                        MessageRole::User,
                        "unopened history",
                    )],
                ),
            )
            .unwrap();
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let conversation = state.conversation(project, first);
                conversation
                    .lock()
                    .unwrap()
                    .append_streaming("partial answer");
                let field = state.draft(first, cx);
                field.update(cx, |field, cx| field.set_value("unfinished draft", cx));
                state.save_all(cx);
                assert_eq!(
                    conversation.lock().unwrap().message_count(),
                    0,
                    "checkpoint must not split a live streaming segment"
                );
            })
        });
        let persisted = store.load_conversation(project, first).unwrap().unwrap();
        assert_eq!(persisted.messages[0].text_content(), "partial answer");
        assert_eq!(persisted.draft_text, "unfinished draft");
        assert_eq!(
            store
                .load_conversation(project, unopened)
                .unwrap()
                .unwrap()
                .messages[0]
                .text_content(),
            "unopened history"
        );
    }

    #[test]
    fn unreadable_saved_transcripts_cannot_be_overwritten_by_an_empty_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        let path = paths.conversation_file(project, first);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "broken primary").unwrap();
        std::fs::write(fx_store::write::backup_path(&path), "broken backup").unwrap();
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, first);
                assert!(state.last_error.is_some());
                assert!(!state.prepare_prompt(first, "would overwrite", cx));
                state.save_all(cx);
            })
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken primary");
        assert_eq!(
            std::fs::read_to_string(fx_store::write::backup_path(&path)).unwrap(),
            "broken backup"
        );
    }

    #[test]
    fn a_save_is_queued_off_the_ui_thread_and_a_newer_one_replaces_it() {
        // Each checkpoint used to clone, encode and write the transcript on the
        // UI thread, once a second while a turn streamed, and kept every saved
        // transcript's encoded bytes alive to compare the next one against.
        use std::sync::atomic::Ordering::Relaxed;

        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let file = paths.conversation_file(project, agent);
                let conversation = state.conversation(project, agent);
                conversation
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::User, "first"));
                state.writer.hold(true);
                state.save_conversations();
                assert_eq!(state.writer.waiting(), 1);
                assert_eq!(state.writer.stats().encodes.load(Relaxed), 0);
                assert!(!file.exists(), "the save ran on the UI thread");

                // A newer snapshot replaces the waiting one; the older can never
                // land after it.
                conversation
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::Assistant, "second"));
                state
                    .draft(agent, cx)
                    .update(cx, |field, cx| field.set_value("newest draft", cx));
                state.capture_drafts(cx);
                state.save_conversations();
                assert_eq!(state.writer.waiting(), 1);
                state.writer.hold(false);
                state.flush_saves();
                assert_eq!(state.writer.stats().writes.load(Relaxed), 1);
                let saved = state
                    .store
                    .load_conversation(project, agent)
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.messages.len(), 2);
                assert_eq!(saved.draft_text, "newest draft");

                // Unchanged: nothing is queued. A revision that changes nothing
                // persisted costs one encode on the writer, and no write.
                state.save_conversations();
                assert_eq!(state.writer.waiting(), 0);
                conversation
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::Running);
                state.save_conversations();
                state.flush_saves();
                assert_eq!(state.writer.stats().encodes.load(Relaxed), 2);
                assert_eq!(state.writer.stats().writes.load(Relaxed), 1);
                assert!(fx_store::write::has_backup(&file));
            })
        });
    }

    #[test]
    fn opening_a_saved_thread_does_not_write_it_back() {
        use std::sync::atomic::Ordering::Relaxed;

        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        let mut saved = PersistedConversation::from_messages(
            agent,
            vec![ConversationMessage::text(MessageRole::User, "on disk")],
        );
        saved.draft_text = "saved draft".into();
        Store::new(paths.clone())
            .save_conversation(project, agent, &saved)
            .unwrap();
        test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, agent);
                assert_eq!(state.draft(agent, cx).read(cx).value(), "saved draft");
                state.save_all(cx);
                assert_eq!(state.writer.stats().encodes.load(Relaxed), 0);
            })
        });
    }

    #[test]
    fn an_oversized_thread_reads_its_previous_cache_once_and_keeps_its_draft() {
        // Every save of a thread over the cache limit used to re-read and
        // re-parse its previous file, on the UI thread, once a second.
        use std::sync::atomic::Ordering::Relaxed;

        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let conversation = state.conversation(project, agent);
                conversation
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::User, "Old cache"));
                state.save_all(cx);
                state.writer.set_limit(2048);
                let field = state.draft(agent, cx);
                for round in 0..3 {
                    conversation
                        .lock()
                        .unwrap()
                        .append(ConversationMessage::text(
                            MessageRole::Assistant,
                            "large".repeat(1000),
                        ));
                    let draft = format!("Keep my typing {round}");
                    field.update(cx, |field, cx| field.set_value(draft.clone(), cx));
                    state.last_error = None;
                    state.save_all(cx);
                    assert!(
                        state
                            .last_error
                            .as_deref()
                            .is_some_and(|error| error.contains("cache limit"))
                    );
                    let saved = state
                        .store
                        .load_conversation(project, agent)
                        .unwrap()
                        .unwrap();
                    assert_eq!(saved.messages.len(), 1);
                    assert_eq!(saved.messages[0].text_content(), "Old cache");
                    assert_eq!(saved.draft_text, draft);
                }
                assert_eq!(state.writer.stats().previous_reads.load(Relaxed), 1);
            })
        });
    }

    #[test]
    fn deleting_a_thread_drops_its_waiting_save() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state
                    .conversation(project, first)
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::User, "delete me"));
                state.writer.hold(true);
                state.save_conversations();
                assert_eq!(state.writer.waiting(), 1);
                assert!(state.delete_thread(first));
                assert_eq!(state.writer.waiting(), 0);
                state.writer.hold(false);
                state.flush_saves();
                assert!(!paths.conversation_file(project, first).exists());
            })
        });
    }

    #[test]
    fn unreadable_project_snapshots_are_reported_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        paths.ensure().unwrap();
        std::fs::write(paths.projects_file(), "broken primary").unwrap();
        std::fs::write(
            fx_store::write::backup_path(&paths.projects_file()),
            "broken backup",
        )
        .unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths.clone(), cx));
            state.update(cx, |state, cx| {
                assert!(state.last_error.is_some());
                state.save_all(cx);
            });
        });
        assert_eq!(
            std::fs::read_to_string(paths.projects_file()).unwrap(),
            "broken primary"
        );
        assert_eq!(
            std::fs::read_to_string(fx_store::write::backup_path(&paths.projects_file())).unwrap(),
            "broken backup"
        );
    }

    #[test]
    fn updates_belong_to_the_originating_thread_after_selection_changes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let first_conversation = state.conversation(project, first);
                let second_conversation = state.conversation(project, second);
                let first_updates = state.update_sender_for(first);
                state.select_agent(second);
                first_updates(SessionUpdate::Started {
                    thread_id: "first-session".into(),
                });
                state.drain_updates();
                assert_eq!(
                    first_conversation.lock().unwrap().session_id(),
                    Some("first-session")
                );
                assert_eq!(second_conversation.lock().unwrap().session_id(), None);
            })
        });
    }

    #[test]
    fn interaction_requests_remain_scoped_until_resolved_or_their_turn_ends() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, first, second) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let request = |id: &str| SessionUpdate::Interaction {
                    event: fx_agent::ProviderEvent::UserInput(
                        fx_agent::provider::UserInputRequest {
                            id: id.into(),
                            questions: vec![],
                        },
                    ),
                };
                let first_updates = state.update_sender_for(first);
                let second_updates = state.update_sender_for(second);
                first_updates(request("first-question"));
                first_updates(request("first-question"));
                second_updates(request("second-question"));
                state.select_agent(second);
                state.drain_updates();
                assert_eq!(state.pending_interactions[&first].len(), 1);
                assert_eq!(state.next_question().unwrap().0, second);
                second_updates(SessionUpdate::InteractionResolved {
                    id: "second-question".into(),
                });
                state.drain_updates();
                assert_eq!(state.next_question().unwrap().0, first);
                first_updates(SessionUpdate::Failed {
                    reason: "process exited".into(),
                });
                state.drain_updates();
                assert!(state.next_question().is_none());
                assert!(!state.respond_to_interaction(
                    first,
                    fx_agent::provider::UserResponse::CancelQuestions {
                        id: "first-question".into()
                    }
                ));
            })
        });
    }

    #[test]
    fn an_approval_is_answered_once_and_never_through_the_question_dialog() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, _, agent, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let approval = |approved| fx_agent::provider::UserResponse::Approval {
                    id: "approval".into(),
                    approved,
                };
                let updates = state.update_sender_for(agent);
                updates(SessionUpdate::Interaction {
                    event: fx_agent::ProviderEvent::Approval(fx_agent::provider::ApprovalRequest {
                        id: "approval".into(),
                        tool_name: "Bash".into(),
                        input: serde_json::json!({ "command": "cargo test" }),
                        risk: fx_agent::provider::RiskLevel::Moderate,
                    }),
                });
                state.drain_updates();
                assert!(state.next_question().is_none(), "approvals have no dialog");
                assert!(
                    !state.awaits_answer(agent, "approval"),
                    "with no turn running, nothing would hear the answer"
                );

                let (runner, mut received) = crate::session::TurnRunner::detached();
                state.runners.insert(agent, runner);
                assert!(state.awaits_answer(agent, "approval"));
                assert!(!state.awaits_answer(agent, "another"));
                assert!(state.respond_to_interaction(agent, approval(true)));
                assert!(!state.awaits_answer(agent, "approval"));
                assert!(
                    !state.respond_to_interaction(agent, approval(false)),
                    "a second click reached the provider"
                );
                assert_eq!(received.try_recv().ok(), Some(approval(true)));
                assert!(received.try_recv().is_err());
            })
        });
    }
}

#[cfg(test)]
mod native_sync_tests {
    use super::*;
    use crate::native_sync::{Completion, Payload, RequestKind, Ticket};
    use fx_agent::native::{NativeThread, NativeThreadIndex, NativeThreadSummary};
    use fx_core::{ConversationMessage, MessageRole, NativeDefaults};

    fn setup(cx: &mut App, paths: AppPaths, root: &str) -> (Entity<AppState>, Uuid) {
        let state = cx.new(|cx| AppState::restore(paths, cx));
        let project = state.update(cx, |state, _| {
            state.add_project(root);
            state.snapshot.active_project_id.unwrap()
        });
        (state, project)
    }

    fn entry(root: &str, provider: &str, session: &str, title: &str) -> NativeThreadSummary {
        NativeThreadSummary {
            summary: fx_agent::ThreadSummary {
                identity: fx_agent::ThreadIdentity {
                    provider_id: provider.into(),
                    session_id: session.into(),
                },
                title: title.into(),
                working_directory: root.into(),
                updated_at: fx_core::models::now(),
                is_archived: false,
            },
            defaults: NativeDefaults {
                provider_id: Some(provider.into()),
                model_id: Some("native-model".into()),
                effort: Some("high".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
                context_window_size: Some(200_000),
            },
            revision: "revision-1".into(),
        }
    }

    fn index(
        state: &mut AppState,
        project: Uuid,
        provider: &str,
        entries: Vec<NativeThreadSummary>,
        sessions: Vec<(Uuid, String)>,
    ) {
        state.apply_native_completion(Completion {
            ticket: Ticket {
                project,
                root: state.project(project).unwrap().root_path.clone(),
                provider: provider.into(),
                generation: state.native_sync.generation(project),
                kind: RequestKind::Index {
                    candidates: Vec::new(),
                    missing_candidates: Vec::new(),
                },
            },
            result: Ok(Payload::Index {
                index: NativeThreadIndex {
                    threads: entries,
                    warnings: Vec::new(),
                },
                sessions,
                protected: Vec::new(),
            }),
        });
    }

    fn history_ticket(state: &mut AppState, project: Uuid, agent: Uuid) -> Ticket {
        let binding = state
            .snapshot
            .projects
            .iter()
            .flat_map(|record| &record.native_bindings)
            .find(|binding| binding.agent_id == agent)
            .unwrap()
            .clone();
        let revision = state
            .conversation(project, agent)
            .lock()
            .unwrap()
            .revision();
        Ticket {
            project,
            root: state.project(project).unwrap().root_path.clone(),
            provider: binding.provider_id,
            generation: state.native_sync.generation(project),
            kind: RequestKind::History {
                agent,
                session: binding.session_id,
                revision,
                known_revision: None,
            },
        }
    }

    fn history(
        state: &mut AppState,
        ticket: Ticket,
        entry: NativeThreadSummary,
        messages: Vec<ConversationMessage>,
    ) {
        state.apply_native_completion(Completion {
            ticket,
            result: Ok(Payload::History(NativeThread {
                entry,
                messages,
                current_context_tokens: Some(123),
                history_truncated: false,
            })),
        });
    }

    #[test]
    fn discovery_reuses_disk_session_and_isolates_provider_project_and_deleted_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let paths = AppPaths::at(temp.path().join("store"));
        let test = gpui::TestAppContext::single();
        let (state, project) = test.update(|cx| setup(cx, paths.clone(), &root));
        let (codex_id, claude_id) = test.update(|cx| {
            state.update(cx, |state, _| {
                let codex_id = state.create_thread().unwrap();
                let mut cached = PersistedConversation::from_messages(
                    codex_id,
                    vec![ConversationMessage::text(
                        MessageRole::User,
                        "Saved locally",
                    )],
                );
                cached.session_id = Some("same-session".into());
                state
                    .store
                    .save_conversation(project, codex_id, &cached)
                    .unwrap();
                state.conversations.remove(&codex_id);
                state.snapshot.projects[0].agents[0]
                    .configuration
                    .agent_access = Some(fx_core::AgentAccess::AcceptEdits);
                let good = entry(&root, "codex", "same-session", "Remote title");
                index(
                    state,
                    project,
                    "codex",
                    vec![
                        good,
                        entry("/some/other/project", "codex", "wrong", "Wrong"),
                        entry(&root, "claude", "wrong-provider", "Wrong"),
                    ],
                    vec![(codex_id, "same-session".into())],
                );
                assert_eq!(state.snapshot.projects[0].agents.len(), 1);
                assert_eq!(state.snapshot.active_agent_id, Some(codex_id));
                assert_eq!(
                    state.thread(codex_id).unwrap().configuration.agent_access,
                    Some(fx_core::AgentAccess::AcceptEdits)
                );
                assert_eq!(
                    state.native_defaults(codex_id).agent_access,
                    Some(fx_core::AgentAccess::Supervised)
                );
                index(
                    state,
                    project,
                    "claude",
                    vec![entry(&root, "claude", "same-session", "Claude")],
                    Vec::new(),
                );
                let claude_id = state.snapshot.projects[0]
                    .native_bindings
                    .iter()
                    .find(|binding| binding.provider_id == "claude")
                    .unwrap()
                    .agent_id;
                assert_ne!(codex_id, claude_id);
                assert_eq!(
                    state
                        .thread(claude_id)
                        .unwrap()
                        .configuration
                        .provider_id
                        .as_deref(),
                    Some("claude")
                );
                assert!(state.delete_thread(codex_id));
                assert_eq!(
                    state.snapshot.projects[0].suppressed_native_threads.len(),
                    1
                );
                (codex_id, claude_id)
            })
        });
        test.update(|cx| {
            let reopened = cx.new(|cx| AppState::restore(paths, cx));
            reopened.update(cx, |state, _| {
                index(
                    state,
                    project,
                    "codex",
                    vec![entry(&root, "codex", "same-session", "Would resurrect")],
                    Vec::new(),
                );
                assert!(state.thread(codex_id).is_none());
                assert!(state.thread(claude_id).is_some());
                assert_eq!(state.snapshot.projects[0].agents.len(), 1);
                assert!(
                    !state.native_sync_enabled(),
                    "view tests never launch installed CLIs"
                );
            });
        });
    }

    #[test]
    fn full_history_and_external_rewrites_preserve_drafts_and_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let paths = AppPaths::at(temp.path().join("store"));
        let test = gpui::TestAppContext::single();
        let (state, project) = test.update(|cx| setup(cx, paths.clone(), &root));
        let agent = test.update(|cx| {
            state.update(cx, |state, cx| {
                let entry = entry(&root, "claude", "native", "Imported");
                index(state, project, "claude", vec![entry.clone()], Vec::new());
                let agent = state.snapshot.active_agent_id.unwrap();
                let ticket = history_ticket(state, project, agent);
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .session_id(),
                    Some("native")
                );
                state
                    .draft(agent, cx)
                    .update(cx, |draft, cx| draft.set_value("Unsent typing", cx));
                state.capture_drafts(cx);
                let messages: Vec<_> = (0..900)
                    .map(|index| {
                        ConversationMessage::text(
                            MessageRole::Assistant,
                            format!("message {index}"),
                        )
                    })
                    .collect();
                history(state, ticket, entry.clone(), messages);
                let conversation = state.conversation(project, agent);
                assert_eq!(conversation.lock().unwrap().message_count(), 900);
                conversation
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(
                        MessageRole::Assistant,
                        "Continued",
                    ));
                assert_eq!(
                    conversation.lock().unwrap().message_count(),
                    901,
                    "continuing native history keeps its oldest rows"
                );
                let ticket = history_ticket(state, project, agent);
                history(
                    state,
                    ticket,
                    entry,
                    vec![ConversationMessage::text(
                        MessageRole::User,
                        "External rewrite",
                    )],
                );
                assert_eq!(conversation.lock().unwrap().message_count(), 1);
                assert_eq!(
                    conversation.lock().unwrap().last_text().as_deref(),
                    Some("External rewrite")
                );
                assert_eq!(state.draft(agent, cx).read(cx).value(), "Unsent typing");
                state.save_all(cx);
                agent
            })
        });
        test.update(|cx| {
            let reopened = cx.new(|cx| AppState::restore(paths, cx));
            reopened.update(cx, |state, cx| {
                assert_eq!(state.snapshot.active_agent_id, Some(agent));
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .session_id(),
                    Some("native")
                );
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .last_text()
                        .as_deref(),
                    Some("External rewrite")
                );
                assert_eq!(state.draft(agent, cx).read(cx).value(), "Unsent typing");
            });
        });
    }

    #[test]
    fn late_history_cannot_replace_busy_completed_unsent_or_removed_threads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let entry = entry(&root, "codex", "native", "Imported");
                index(state, project, "codex", vec![entry.clone()], Vec::new());
                let agent = state.snapshot.active_agent_id.unwrap();
                let stale_ticket = history_ticket(state, project, agent);
                assert!(state.prepare_prompt(agent, "Newest question", cx));
                history(
                    state,
                    stale_ticket,
                    entry.clone(),
                    vec![ConversationMessage::text(
                        MessageRole::Assistant,
                        "Old response",
                    )],
                );
                let conversation = state.conversation(project, agent);
                assert_eq!(
                    conversation.lock().unwrap().last_text().as_deref(),
                    Some("Newest question")
                );
                conversation.lock().unwrap().clear_unsent();
                conversation
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(
                        MessageRole::Assistant,
                        "Newest answer",
                    ));
                state.mark_native_local_turn(agent);
                let current_ticket = history_ticket(state, project, agent);
                history(
                    state,
                    current_ticket,
                    entry.clone(),
                    vec![ConversationMessage::text(
                        MessageRole::User,
                        "Older question",
                    )],
                );
                assert_eq!(
                    conversation.lock().unwrap().last_text().as_deref(),
                    Some("Newest answer")
                );
                assert!(
                    state
                        .native_status(project)
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("caught up"))
                );
                let current_ticket = history_ticket(state, project, agent);
                history(
                    state,
                    current_ticket,
                    entry.clone(),
                    vec![
                        ConversationMessage::text(MessageRole::User, "Newest question"),
                        ConversationMessage::text(MessageRole::Assistant, "Newest answer"),
                    ],
                );
                assert!(!state.snapshot.projects[0].native_bindings[0].local_history_pending);
                let current_ticket = history_ticket(state, project, agent);
                conversation
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::Running);
                history(
                    state,
                    current_ticket,
                    entry.clone(),
                    vec![ConversationMessage::text(
                        MessageRole::Assistant,
                        "Busy replacement",
                    )],
                );
                assert_eq!(
                    conversation.lock().unwrap().last_text().as_deref(),
                    Some("Newest answer")
                );
                conversation
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::Idle);
                let late_ticket = history_ticket(state, project, agent);
                state.remove_project(project);
                history(
                    state,
                    late_ticket,
                    entry,
                    vec![ConversationMessage::text(
                        MessageRole::Assistant,
                        "Removed project replacement",
                    )],
                );
                assert!(state.snapshot.projects.is_empty());
                assert_eq!(
                    conversation.lock().unwrap().last_text().as_deref(),
                    Some("Newest answer")
                );
            })
        });
    }

    #[test]
    fn remote_metadata_updates_unless_locally_overridden_and_failed_read_keeps_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let mut entry = entry(&root, "codex", "native", "Original");
                index(state, project, "codex", vec![entry.clone()], Vec::new());
                let agent = state.snapshot.active_agent_id.unwrap();
                entry.summary.title = "Remote rename".into();
                entry.summary.is_archived = true;
                index(state, project, "codex", vec![entry.clone()], Vec::new());
                assert_eq!(state.thread(agent).unwrap().title, "Remote rename");
                assert!(state.is_thread_archived(agent));
                assert!(state.rename_thread(agent, "Local name"));
                assert!(state.set_thread_archived(agent, false));
                index(state, project, "codex", vec![entry], Vec::new());
                assert_eq!(state.thread(agent).unwrap().title, "Local name");
                assert!(!state.is_thread_archived(agent));
                let ticket = history_ticket(state, project, agent);
                state.apply_native_completion(Completion {
                    ticket,
                    result: Err("offline".into()),
                });
                assert_eq!(
                    state
                        .conversation(project, agent)
                        .lock()
                        .unwrap()
                        .session_id(),
                    Some("native")
                );
                assert!(
                    state
                        .native_status(project)
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("offline"))
                );
                assert_eq!(state.snapshot.projects[0].native_bindings.len(), 1);
            })
        });
    }
    #[test]
    fn missing_native_threads_hide_only_after_complete_index_and_keep_local_drafts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let first = entry(&root, "codex", "first", "First");
                let second = entry(&root, "codex", "second", "Second");
                index(
                    state,
                    project,
                    "codex",
                    vec![first.clone(), second.clone()],
                    Vec::new(),
                );
                let first_id = state.snapshot.active_agent_id.unwrap();
                let second_id = state.snapshot.projects[0]
                    .native_bindings
                    .iter()
                    .find(|binding| binding.session_id == "second")
                    .unwrap()
                    .agent_id;
                // A partial index cannot prove that a thread was deleted.
                let ticket = Ticket {
                    project,
                    root: root.clone(),
                    provider: "codex".into(),
                    generation: state.native_sync.generation(project),
                    kind: RequestKind::Index {
                        candidates: Vec::new(),
                        missing_candidates: Vec::new(),
                    },
                };
                state.apply_native_completion(Completion {
                    ticket: ticket.clone(),
                    result: Ok(Payload::Index {
                        index: NativeThreadIndex {
                            threads: vec![first.clone()],
                            warnings: vec!["One file unreadable".into()],
                        },
                        sessions: Vec::new(),
                        protected: Vec::new(),
                    }),
                });
                assert!(
                    state.snapshot.projects[0]
                        .missing_native_agent_ids
                        .is_empty()
                );
                // The worker checks an unopened missing thread's disk cache for drafts.
                state.apply_native_completion(Completion {
                    ticket,
                    result: Ok(Payload::Index {
                        index: NativeThreadIndex {
                            threads: vec![first.clone()],
                            warnings: Vec::new(),
                        },
                        sessions: Vec::new(),
                        protected: vec![second_id],
                    }),
                });
                assert!(
                    state.snapshot.projects[0]
                        .missing_native_agent_ids
                        .is_empty()
                );
                index(state, project, "codex", vec![first.clone()], Vec::new());
                assert_eq!(
                    state.snapshot.projects[0].missing_native_agent_ids,
                    vec![second_id]
                );
                assert!(
                    state.thread(second_id).is_some(),
                    "configuration and binding survive disappearance"
                );
                assert_eq!(
                    state
                        .active_agents()
                        .iter()
                        .map(|agent| agent.id)
                        .collect::<Vec<_>>(),
                    vec![first_id]
                );
                index(state, project, "codex", vec![first, second], Vec::new());
                assert!(
                    state.snapshot.projects[0]
                        .missing_native_agent_ids
                        .is_empty()
                );
                assert_eq!(
                    state.snapshot.projects[0].agents.len(),
                    2,
                    "reappearance reuses the same presentation"
                );
            })
        });
    }

    #[test]
    fn oversized_history_recovery_keeps_draft_unsent_and_previous_cache() {
        let agent = Uuid::new_v4();
        let old = PersistedConversation::from_messages(
            agent,
            vec![ConversationMessage::text(MessageRole::User, "Old cache")],
        );
        let mut incoming = PersistedConversation::from_messages(
            agent,
            vec![ConversationMessage::text(
                MessageRole::Assistant,
                "large".repeat(4000),
            )],
        );
        incoming.session_id = Some("native-session".into());
        incoming.draft_text = "Keep my local typing".into();
        incoming.unsent_prompt = Some(fx_store::PersistedUnsentPrompt::new(
            "Failed send".into(),
            0,
            Some("native-session".into()),
        ));
        let (partial, bytes) = encode_partial_cache(incoming, Some(old), 2048).unwrap();
        assert!(bytes.len() <= 2048);
        assert_eq!(partial.messages[0].text_content(), "Old cache");
        assert_eq!(partial.draft_text, "Keep my local typing");
        assert_eq!(partial.unsent_prompt.unwrap().text, "Failed send");
        assert_eq!(partial.session_id.as_deref(), Some("native-session"));
        let reopened: PersistedConversation = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reopened.draft_text, "Keep my local typing");
    }

    #[test]
    fn fake_worker_pipeline_prioritizes_selected_history_and_throttles_unchanged_reads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| state.update(cx, |state, cx| {
            let native_entry = entry(&root, "codex", "native", "Imported");
            index(state, project, "codex", vec![native_entry.clone()], Vec::new());
            let agent = state.snapshot.active_agent_id.unwrap();
            let (codex_jobs, codex_results) = state.native_sync.test_worker("codex");
            let (claude_jobs, _claude_results) = state.native_sync.test_worker("claude");
            // The thread has no history yet, so its loading state appears.
            assert!(state.poll_native_sync(cx));
            let initial = codex_jobs.try_recv().unwrap();
            assert!(matches!(initial.kind, RequestKind::History { agent: id, known_revision: None, .. } if id == agent), "saved native selection loads before startup index jobs");
            let other_provider = claude_jobs.try_recv().unwrap();
            assert!(matches!(other_provider.kind, RequestKind::Index { candidates, missing_candidates } if candidates.is_empty() && missing_candidates.is_empty()));
            // Pending work remains bounded even while many timer ticks pass.
            for _ in 0..20 { state.poll_native_sync(cx); }
            assert!(codex_jobs.try_recv().is_err());
            codex_results.send(Completion { ticket: initial, result: Ok(Payload::History(NativeThread { entry: native_entry, messages: vec![ConversationMessage::text(MessageRole::Assistant, "Native history")], current_context_tokens: None, history_truncated: false })) }).unwrap();
            state.poll_native_sync(cx);
            let index_ticket = codex_jobs.try_recv().unwrap();
            assert!(matches!(index_ticket.kind, RequestKind::Index { .. }), "metadata gets a turn after selected history");
            codex_results.send(Completion { ticket: index_ticket, result: Ok(Payload::Index { index: NativeThreadIndex::default(), sessions: Vec::new(), protected: Vec::new() }) }).unwrap();
            state.poll_native_sync(cx);
            assert!(codex_jobs.try_recv().is_err(), "normal history polling is throttled");
            state.native_sync.history_due.insert(agent, Instant::now() - Duration::from_secs(1));
            state.poll_native_sync(cx);
            let repeat = codex_jobs.try_recv().unwrap();
            assert!(matches!(&repeat.kind, RequestKind::History { known_revision: Some(revision), .. } if revision == "revision-1"));
            let revision = state.conversation(project, agent).lock().unwrap().revision();
            codex_results.send(Completion { ticket: repeat, result: Ok(Payload::Unchanged) }).unwrap();
            state.poll_native_sync(cx);
            assert_eq!(state.conversation(project, agent).lock().unwrap().revision(), revision);
        }));
    }
    #[test]
    fn a_sync_pass_that_finds_nothing_new_does_not_redraw_the_window() {
        // Every completed pass used to report a change, so the whole window
        // redrew every second or two and the sync row blinked.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                let known = entry(&root, "codex", "known", "Known thread");
                index(state, project, "codex", vec![known.clone()], Vec::new());
                // History already on screen, so a running pass shows nothing.
                let agent = state.snapshot.active_agent_id.unwrap();
                state
                    .conversation(project, agent)
                    .lock()
                    .unwrap()
                    .append(ConversationMessage::text(MessageRole::Assistant, "Earlier"));
                let (codex_jobs, codex_results) = state.native_sync.test_worker("codex");
                let (_claude_jobs, _claude_results) = state.native_sync.test_worker("claude");
                assert!(
                    !state.poll_native_sync(cx),
                    "starting a pass redrew the window"
                );
                let history = codex_jobs.try_recv().unwrap();
                codex_results
                    .send(Completion {
                        ticket: history,
                        result: Ok(Payload::Unchanged),
                    })
                    .unwrap();
                assert!(
                    !state.poll_native_sync(cx),
                    "unchanged history redrew the window"
                );

                let same = codex_jobs.try_recv().unwrap();
                assert!(matches!(same.kind, RequestKind::Index { .. }));
                let reply = |entries| {
                    Ok(Payload::Index {
                        index: NativeThreadIndex {
                            threads: entries,
                            warnings: Vec::new(),
                        },
                        sessions: Vec::new(),
                        protected: Vec::new(),
                    })
                };
                codex_results
                    .send(Completion {
                        ticket: same,
                        result: reply(vec![known.clone()]),
                    })
                    .unwrap();
                assert!(
                    !state.poll_native_sync(cx),
                    "an identical index redrew the window"
                );

                state.native_sync.index_due.insert(
                    (project, "codex".into()),
                    Instant::now() - Duration::from_secs(1),
                );
                state.poll_native_sync(cx);
                let next = codex_jobs.try_recv().unwrap();
                let added = entry(&root, "codex", "added", "A new thread");
                codex_results
                    .send(Completion {
                        ticket: next,
                        result: reply(vec![known, added]),
                    })
                    .unwrap();
                assert!(state.poll_native_sync(cx), "a new thread went undrawn");
            })
        });
    }

    #[test]
    fn an_earlier_identical_turn_cannot_clear_the_pending_local_history_guard() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().to_string();
        let test = gpui::TestAppContext::single();
        let (state, project) =
            test.update(|cx| setup(cx, AppPaths::at(temp.path().join("store")), &root));
        test.update(|cx| {
            state.update(cx, |state, _| {
                let entry = entry(&root, "codex", "repeated", "Repeated turns");
                index(state, project, "codex", vec![entry.clone()], Vec::new());
                let agent = state.snapshot.active_agent_id.unwrap();
                let conversation = state.conversation(project, agent);
                let one_turn = vec![
                    ConversationMessage::text(MessageRole::User, "continue"),
                    ConversationMessage::text(MessageRole::Assistant, "Done."),
                ];
                let two_turns = [one_turn.clone(), one_turn.clone()].concat();
                conversation
                    .lock()
                    .unwrap()
                    .replace_full_history(two_turns.clone());
                state.mark_native_local_turn(agent);
                let ticket = history_ticket(state, project, agent);
                history(state, ticket, entry.clone(), one_turn);
                assert!(state.snapshot.projects[0].native_bindings[0].local_history_pending);
                assert_eq!(
                    conversation.lock().unwrap().message_count(),
                    4,
                    "the second identical turn must not be replaced by the first"
                );
                assert!(
                    state
                        .native_status(project)
                        .warnings
                        .iter()
                        .any(|warning| warning.contains("caught up"))
                );
                let ticket = history_ticket(state, project, agent);
                history(state, ticket, entry, two_turns);
                assert!(!state.snapshot.projects[0].native_bindings[0].local_history_pending);
                assert_eq!(conversation.lock().unwrap().message_count(), 4);
                assert!(state.native_status(project).warnings.is_empty());
            })
        });
    }

    #[test]
    fn native_catchup_preserves_role_order_and_normalizes_split_prose() {
        let mut conversation = Conversation::new(Uuid::new_v4());
        conversation.replace_full_history(vec![
            ConversationMessage::text(MessageRole::User, "Check this"),
            ConversationMessage::text(MessageRole::Assistant, "Checking the file."),
            ConversationMessage::text(MessageRole::Tool, "Tool output"),
            ConversationMessage::text(MessageRole::Assistant, "Done."),
        ]);
        assert!(native_contains_local_tail(
            &conversation,
            &[
                ConversationMessage::text(MessageRole::User, "Check this"),
                ConversationMessage::text(MessageRole::Assistant, "Checking the file.\n\nDone."),
            ]
        ));
        assert!(!native_contains_local_tail(
            &conversation,
            &[
                ConversationMessage::text(MessageRole::Assistant, "Checking the file. Done."),
                ConversationMessage::text(MessageRole::User, "Check this"),
            ]
        ));
        conversation.replace_full_history(vec![
            ConversationMessage::text(MessageRole::User, "continue"),
            ConversationMessage::text(MessageRole::Assistant, "Done."),
            ConversationMessage::text(MessageRole::Assistant, "Done."),
        ]);
        assert!(
            !native_contains_local_tail(
                &conversation,
                &[
                    ConversationMessage::text(MessageRole::User, "continue"),
                    ConversationMessage::text(MessageRole::Assistant, "Done."),
                ]
            ),
            "one native prose occurrence cannot satisfy two local segments"
        );
    }
}

#[cfg(test)]
mod project_lifecycle_tests {
    use super::*;
    use fx_store::PersistedProject;

    fn project_at(root: &std::path::Path, agents: &[Agent]) -> PersistedProject {
        let mut project = Project::new(root.to_string_lossy());
        project.agent_order = agents.iter().map(|agent| agent.id).collect();
        PersistedProject {
            project: Some(project),
            agents: agents.to_vec(),
            is_expanded: true,
            ..Default::default()
        }
    }

    #[test]
    fn adding_an_open_folder_again_focuses_it_instead_of_duplicating_it() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let link = temp.path().join("first-link");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let paths = AppPaths::at(temp.path().join("state"));
        // Saved before roots were resolved: the stored spelling is the link.
        Store::new(paths.clone())
            .save_app(&PersistedApp {
                projects: vec![project_at(&link, &[])],
                ..Default::default()
            })
            .unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths.clone(), cx));
            state.update(cx, |state, _| {
                let existing = state.snapshot.projects[0].project.as_ref().unwrap().id;
                let resolved = first.canonicalize().unwrap();
                let other = state.add_project(&second.canonicalize().unwrap().to_string_lossy());
                assert_ne!(other, existing);
                state.toggle_project_expansion(existing);
                assert!(state.open_project_roots().contains(&resolved));

                for spelling in [
                    resolved.to_string_lossy().into_owned(),
                    format!("{}/", resolved.display()),
                    link.to_string_lossy().into_owned(),
                ] {
                    assert_eq!(state.add_project(&spelling), existing, "{spelling}");
                    assert_eq!(state.snapshot.projects.len(), 2, "{spelling}");
                    assert_eq!(state.snapshot.active_project_id, Some(existing));
                    assert!(state.is_expanded(existing));
                    state.select_project(other);
                }
            });
        });
        assert_eq!(Store::new(paths).load_app().unwrap().projects.len(), 2);
    }

    #[test]
    fn removing_a_project_releases_its_threads_and_keeps_their_files() {
        let temp = tempfile::tempdir().unwrap();
        let removed_root = temp.path().join("removed");
        let kept_root = temp.path().join("kept");
        std::fs::create_dir_all(&removed_root).unwrap();
        std::fs::create_dir_all(&kept_root).unwrap();
        let (first, second, kept) = (
            Agent::new("First"),
            Agent::new("Second"),
            Agent::new("Kept"),
        );
        let removed = project_at(&removed_root, &[first.clone(), second.clone()]);
        let removed_id = removed.project.as_ref().unwrap().id;
        let survivor = project_at(&kept_root, std::slice::from_ref(&kept));
        let kept_id = survivor.project.as_ref().unwrap().id;
        let paths = AppPaths::at(temp.path().join("state"));
        Store::new(paths.clone())
            .save_app(&PersistedApp {
                active_project_id: Some(removed_id),
                active_agent_id: Some(first.id),
                projects: vec![removed, survivor],
                ..Default::default()
            })
            .unwrap();
        let test = gpui::TestAppContext::single();
        test.update(|cx| {
            let state = cx.new(|cx| AppState::restore(paths.clone(), cx));
            state.update(cx, |state, cx| {
                for (project, agent) in [
                    (removed_id, first.id),
                    (removed_id, second.id),
                    (kept_id, kept.id),
                ] {
                    state.conversation(project, agent);
                    let draft = state.draft(agent, cx);
                    draft.update(cx, |field, cx| field.set_value("Unsent work", cx));
                }
                state
                    .conversation(removed_id, first.id)
                    .lock()
                    .unwrap()
                    .append(fx_core::ConversationMessage::text(
                        fx_core::MessageRole::User,
                        "Saved prompt",
                    ));
                // A naming request still in flight, and leftovers of a turn.
                state.title_jobs.insert(
                    first.id,
                    crate::auto_title::TitleJob {
                        generation: Uuid::new_v4(),
                        provider: "codex".into(),
                        model: "model".into(),
                    },
                );
                state
                    .pending_interactions
                    .insert(second.id, VecDeque::new());
                state.retry_drafts.insert(second.id, "Retry".into());
                state.restored_costs.insert(second.id, 1.0);
                state.protected_conversations.insert(second.id);
                state
                    .native_sync
                    .history_warnings
                    .insert(first.id, "warning".into());
                state
                    .native_sync
                    .completed_turns
                    .insert(first.id, Instant::now());
                state.save_all(cx);
                assert!(state.saved_conversations.contains_key(&first.id));

                // A turn still waiting on the reader keeps its project.
                state
                    .conversation(kept_id, kept.id)
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::WaitingForInput);
                state.remove_project(kept_id);
                assert!(state.project(kept_id).is_some());
                assert!(state.last_error.is_some());
                state
                    .conversation(kept_id, kept.id)
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::Idle);

                state.remove_project(removed_id);
                assert!(state.project(removed_id).is_none());
                assert_eq!(state.snapshot.active_project_id, Some(kept_id));
                for agent in [first.id, second.id] {
                    assert!(!state.conversations.contains_key(&agent));
                    assert!(!state.drafts.contains_key(&agent));
                    assert!(!state.saved_conversations.contains_key(&agent));
                    assert!(!state.title_jobs.contains_key(&agent));
                    assert!(!state.pending_interactions.contains_key(&agent));
                    assert!(!state.restored_drafts.contains_key(&agent));
                    assert!(!state.retry_drafts.contains_key(&agent));
                    assert!(!state.restored_costs.contains_key(&agent));
                    assert!(!state.protected_conversations.contains(&agent));
                    assert!(!state.native_sync.history_warnings.contains_key(&agent));
                    assert!(!state.native_sync.completed_turns.contains_key(&agent));
                }
                // The surviving project's thread is untouched.
                assert!(state.conversations.contains_key(&kept.id));
                assert!(state.drafts.contains_key(&kept.id));
                assert!(state.saved_conversations.contains_key(&kept.id));
            });
        });
        // Removal is from the sidebar only: the transcript and its draft stay.
        let saved = Store::new(paths)
            .load_conversation(removed_id, first.id)
            .unwrap()
            .expect("the removed project's transcript is kept");
        assert_eq!(saved.draft_text, "Unsent work");
        assert_eq!(saved.messages.len(), 1);
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;
    use fx_core::{ConversationMessage, MessageContent, MessageRole};

    /// One project with two threads, the first selected, built through the
    /// real restore path.
    fn workspace(cx: &mut App, paths: &AppPaths) -> (Entity<AppState>, Uuid, Uuid, Uuid) {
        let mut project = Project::new(paths.root().parent().unwrap().to_string_lossy());
        let mut first = Agent::new("First");
        first.configuration.provider_id = Some("codex".into());
        first.configuration.model_id = Some("gpt-test".into());
        first.configuration.effort = Some("high".into());
        let second = Agent::new("Second");
        project.agent_order = vec![first.id, second.id];
        let project_id = project.id;
        let snapshot = PersistedApp {
            active_project_id: Some(project.id),
            active_agent_id: Some(first.id),
            projects: vec![fx_store::PersistedProject {
                project: Some(project),
                agents: vec![first.clone(), second.clone()],
                is_expanded: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        Store::new(paths.clone()).save_app(&snapshot).unwrap();
        (
            cx.new(|cx| AppState::restore(paths.clone(), cx)),
            project_id,
            first.id,
            second.id,
        )
    }

    fn say(state: &mut AppState, project: Uuid, agent: Uuid, text: &str) {
        let conversation = state.conversation(project, agent);
        let mut conversation = conversation.lock().unwrap();
        conversation.append(ConversationMessage::new(
            MessageRole::User,
            vec![MessageContent::Text { text: text.into() }],
        ));
    }

    #[test]
    fn resetting_a_flowx_only_thread_empties_it_in_place_and_deletes_its_cache() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                say(state, project, first, "hello");
                state
                    .conversation(project, first)
                    .lock()
                    .unwrap()
                    .set_session_id("local-session");
                let draft = state.draft(first, cx);
                draft.update(cx, |field, cx| field.set_value("unsent", cx));
                state.save_all(cx);
            })
        });
        let store = Store::new(paths.clone());
        assert!(store.load_conversation(project, first).unwrap().is_some());

        let selected =
            test.update(|cx| state.update(cx, |state, cx| state.reset_conversation(first, cx)));
        assert_eq!(selected, Some(first), "the same thread, emptied");
        test.read(|cx| {
            let state = state.read(cx);
            let conversation = state.conversation_by_agent(first).unwrap();
            let conversation = conversation.lock().unwrap();
            assert_eq!(conversation.message_count(), 0);
            assert_eq!(conversation.session_id(), None, "a new session next turn");
            assert_eq!(state.drafts[&first].read(cx).value(), "");
            assert_eq!(state.active_agents().len(), 2, "no thread was added");
        });
        assert!(
            store.load_conversation(project, first).unwrap().is_none(),
            "an empty transcript is deleted, not written as a stub"
        );
    }

    #[test]
    fn resetting_a_provider_thread_starts_a_fresh_one_with_its_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, _| {
                say(state, project, first, "kept by the provider");
                state
                    .conversation(project, first)
                    .lock()
                    .unwrap()
                    .set_session_id("provider-session");
                state.bind_local_session(first);
            })
        });

        let fresh = test
            .update(|cx| state.update(cx, |state, cx| state.reset_conversation(first, cx)))
            .unwrap();
        assert_ne!(fresh, first);
        test.read(|cx| {
            let state = state.read(cx);
            assert_eq!(state.snapshot.active_agent_id, Some(fresh));
            let copied = &state.thread(fresh).unwrap().configuration;
            assert_eq!(copied.provider_id.as_deref(), Some("codex"));
            assert_eq!(copied.model_id.as_deref(), Some("gpt-test"));
            assert_eq!(copied.effort.as_deref(), Some("high"));
            // The provider's thread is left exactly as it was.
            let original = state.conversation_by_agent(first).unwrap();
            assert_eq!(original.lock().unwrap().message_count(), 1);
            assert_eq!(state.active_agents().len(), 3);
        });
        let saved = Store::new(paths).load_app().unwrap();
        assert_eq!(
            saved.active_agent_id,
            Some(fresh),
            "and it survives a restart"
        );
    }

    #[test]
    fn resetting_an_unreadable_thread_keeps_its_files_and_starts_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        let file = paths.conversation_file(project, first);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"{ not a transcript").unwrap();
        let fresh = test.update(|cx| {
            state.update(cx, |state, cx| {
                state.conversation(project, first);
                state.reset_conversation(first, cx)
            })
        });
        assert_ne!(fresh, Some(first));
        assert!(fresh.is_some());
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"{ not a transcript",
            "kept for recovery, not emptied"
        );
    }

    #[test]
    fn a_running_thread_refuses_to_reset() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::at(dir.path().join("state"));
        let test = gpui::TestAppContext::single();
        let (state, project, first, _) = test.update(|cx| workspace(cx, &paths));
        test.update(|cx| {
            state.update(cx, |state, cx| {
                say(state, project, first, "busy");
                state
                    .conversation(project, first)
                    .lock()
                    .unwrap()
                    .set_phase(fx_agent::conversation::Phase::Running);
                assert_eq!(state.reset_conversation(first, cx), None);
                assert!(state.last_error.is_some());
                let conversation = state.conversation_by_agent(first).unwrap();
                assert_eq!(conversation.lock().unwrap().message_count(), 1);
            })
        });
    }
}
