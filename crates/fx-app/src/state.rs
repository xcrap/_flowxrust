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
use fx_store::{
    AppPaths, PersistedApp, PersistedConversation, PersistedPreferences, RightPanelTab, Store,
};

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
    accepted_drafts: HashMap<Uuid, String>,
    saved_conversations: HashMap<Uuid, Vec<u8>>,
    saved_conversation_versions: HashMap<Uuid, (u64, String)>,
    restored_costs: HashMap<Uuid, f64>,
    last_checkpoint: Instant,
    pending_interactions: HashMap<Uuid, VecDeque<fx_agent::ProviderEvent>>,
    /// The add-project sheet, while it is open.
    ///
    /// Owned here rather than by the sidebar so that the menu's "New Project"
    /// item and the sidebar's button open the same sheet, and so it can be
    /// drawn above the whole shell instead of inside one column.
    pub picker: Option<Entity<crate::project_picker::ProjectPicker>>,
    pub sidebar_visible: bool,
    /// The one menu that may be open, app-wide.
    pub menu_layer: Entity<MenuLayer>,
    /// Which screen regions offer a context menu.
    pub context_registry: Entity<ContextRegistry>,
    /// The last save or load failure, kept so the UI can say so.
    pub last_error: Option<String>,
    pub paths: AppPaths,
    native_sync: crate::native_sync::NativeSync,
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
            accepted_drafts: HashMap::new(),
            saved_conversations: HashMap::new(),
            saved_conversation_versions: HashMap::new(),
            restored_costs: HashMap::new(),
            last_checkpoint: Instant::now(),
            pending_interactions: HashMap::new(),
            picker: None,
            sidebar_visible,
            last_error,
            paths,
            native_sync: Default::default(),
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

    /// Naming continues after the main turn without making the composer busy.
    pub fn has_title_jobs(&self) -> bool {
        !self.title_jobs.is_empty()
    }

    /// Discard results from requests started before automatic naming was disabled.
    pub(crate) fn cancel_title_jobs(&mut self) {
        self.title_jobs.clear();
    }

    fn auto_title_record(&self, agent: Uuid) -> Option<&fx_store::PersistedProject> {
        self.snapshot
            .projects
            .iter()
            .find(|record| record.auto_title_pending.contains(&agent))
    }

    fn latest_exchange(&self, agent: Uuid) -> Option<(String, String)> {
        let conversation = self.conversations.get(&agent)?.lock().ok()?;
        let mut user = String::new();
        let mut assistant = String::new();
        for message in conversation.messages() {
            match message.role {
                fx_core::MessageRole::User => {
                    user = message.text_content();
                    assistant.clear();
                }
                fx_core::MessageRole::Assistant => {
                    let text = message.text_content();
                    if !text.trim().is_empty() {
                        assistant = text;
                    }
                }
                _ => {}
            }
        }
        (!user.trim().is_empty()).then_some((user, assistant))
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
        if let Some((user, _)) = self.latest_exchange(agent)
            && let Some(title) = crate::auto_title::fallback_title(&user)
        {
            self.set_auto_title(agent, &title);
        }
    }

    /// Consume eligibility durably before calling a model. Failure, cancellation,
    /// restart and later turns never repeat this automatic paid request.
    fn prepare_auto_title(&mut self, agent_id: Uuid) -> Option<fx_agent::title::TitleRequest> {
        self.auto_title_record(agent_id)?;
        // An opted-out first completion consumes eligibility, but an empty
        // response does not: there is no usable request (or paid attempt) yet.
        let request = if !self.preferences.auto_titles.enabled || self.is_thread_archived(agent_id)
        {
            None
        } else {
            let agent = self.thread(agent_id)?;
            let provider = agent
                .configuration
                .provider_id
                .clone()
                .or(self.native_defaults(agent_id).provider_id)
                .unwrap_or_else(|| "codex".into());
            let (user, assistant) = self.latest_exchange(agent_id)?;
            if assistant.trim().is_empty() {
                return None;
            }
            let model = match provider.as_str() {
                "codex" => self.preferences.auto_titles.codex_model_id.clone(),
                "claude" => self.preferences.auto_titles.claude_model_id.clone(),
                _ => return None,
            };
            Some(fx_agent::title::TitleRequest {
                provider,
                model,
                user,
                assistant,
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
        let Some(agent) = self.thread(agent_id) else {
            return;
        };
        let provider = agent
            .configuration
            .provider_id
            .as_deref()
            .unwrap_or("codex");
        if !self.preferences.auto_titles.enabled
            || self.is_thread_archived(agent_id)
            || provider != job.provider
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
        self.request_native_refresh(id);
    }

    pub fn select_agent(&mut self, id: Uuid) {
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
        self.accepted_drafts.remove(&agent_id);
        self.saved_conversations.remove(&agent_id);
        self.saved_conversation_versions.remove(&agent_id);
        self.restored_costs.remove(&agent_id);
        self.pending_interactions.remove(&agent_id);
        self.native_sync.invalidate(project_id);
        self.native_sync.history_due.remove(&agent_id);
        self.native_sync.history_warnings.remove(&agent_id);
        self.native_sync.completed_turns.remove(&agent_id);
        self.native_sync.loaded_revisions.remove(&agent_id);
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

    pub fn inspector_tab(&self) -> RightPanelTab {
        self.snapshot.right_panel_tab
    }

    pub fn set_inspector_tab(&mut self, tab: RightPanelTab) {
        self.snapshot.right_panel_tab = tab;
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
    pub fn conversation(&mut self, project_id: Uuid, agent_id: Uuid) -> Arc<GuardedConversation> {
        if let Some(existing) = self.conversations.get(&agent_id) {
            return existing.clone();
        }
        let mut conversation = Conversation::new(agent_id);
        match self.store.load_conversation(project_id, agent_id) {
            Ok(Some(persisted)) if persisted.agent_id == agent_id => {
                self.restored_costs
                    .insert(agent_id, persisted.total_cost_usd);
                self.restored_drafts.insert(
                    agent_id,
                    if persisted.draft_text.is_empty() {
                        persisted
                            .unsent_prompt
                            .as_ref()
                            .map(|prompt| prompt.text.clone())
                            .unwrap_or_default()
                    } else {
                        persisted.draft_text.clone()
                    },
                );
                restore_conversation(&mut conversation, persisted);
            }
            Ok(None) => {}
            result => {
                let reason = match result {
                    Err(error) => error.to_string(),
                    _ => "the transcript belongs to a different thread".to_string(),
                };
                self.protected_conversations.insert(agent_id);
                self.last_error = Some(format!(
                    "Could not restore this conversation: {reason}. Its saved files have been preserved."
                ));
                conversation.set_error("The saved conversation could not be read. Its files have been preserved; start a new thread to continue.");
            }
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
        let conversation = Arc::new(Mutex::new(conversation));
        self.conversations.insert(agent_id, conversation.clone());
        conversation
    }

    /// The active thread's conversation, loaded on first use.
    ///
    /// `None` when no thread is selected. Separate from [`Self::conversation`]
    /// because a view asks this every frame and must not trigger a disk read
    /// inside a render.
    pub fn active_conversation(&mut self) -> Option<Arc<GuardedConversation>> {
        let project_id = self.snapshot.active_project_id?;
        let agent_id = self.snapshot.active_agent_id?;
        Some(self.conversation(project_id, agent_id))
    }

    /// The draft field for a thread, creating it on first use.
    pub fn draft(&mut self, agent_id: Uuid, cx: &mut App) -> Entity<TextField> {
        if let Some(existing) = self.drafts.get(&agent_id) {
            return existing.clone();
        }
        if let Some(project_id) = self.project_for_agent(agent_id) {
            self.conversation(project_id, agent_id);
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
    pub fn add_project(&mut self, root_path: &str) {
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
    }

    /// Remove only the sidebar entry. Saved transcripts and project files remain.
    pub fn remove_project(&mut self, project_id: Uuid) {
        if self
            .snapshot
            .projects
            .iter()
            .filter(|record| record.project.as_ref().is_some_and(|p| p.id == project_id))
            .flat_map(|record| &record.agents)
            .any(|agent| self.runners.contains_key(&agent.id))
        {
            self.last_error = Some("Stop this project's running turns before removing it.".into());
            return;
        }
        self.save_conversations();
        self.native_sync.invalidate(project_id);
        self.snapshot
            .projects
            .retain(|record| !record.project.as_ref().is_some_and(|p| p.id == project_id));
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

    /// Persist the user's prompt before starting a worker. A failed launch keeps it retryable.
    pub fn prepare_prompt(&mut self, agent_id: Uuid, text: &str, cx: &App) -> bool {
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
                    message.role == fx_core::MessageRole::User && message.text_content() == text
                });
            if !retrying_same_row {
                conversation.append(fx_core::ConversationMessage::text(
                    fx_core::MessageRole::User,
                    text,
                ));
            }
            let session_id = conversation.session_id().map(str::to_string);
            conversation.hold_unsent(fx_agent::conversation::UnsentPrompt {
                text: text.to_string(),
                attachment_count: 0,
                session_id,
            });
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

    /// Apply anything the running turn has reported.
    ///
    /// Called from the render path, which is the only place that may touch the
    /// entity. A turn finishing must repaint without anything asking it to, so
    /// this runs every pass.
    pub fn drain_updates(&mut self) {
        let updates: Vec<crate::session::SessionUpdate> = self.updates.try_iter().collect();
        for update in updates {
            self.apply_update(self.snapshot.active_agent_id, update);
        }
        let updates: Vec<_> = self.targeted_updates.try_iter().collect();
        for (agent_id, update) in updates {
            self.apply_update(Some(agent_id), update);
        }
    }

    pub fn drain_updates_with_context(&mut self, cx: &mut App) {
        self.drain_updates();
        for (agent_id, accepted) in std::mem::take(&mut self.accepted_drafts) {
            if let Some(field) = self.drafts.get(&agent_id).cloned() {
                // Edits made during startup are the next draft, not the sent prompt.
                if field.read(cx).value().trim() == accepted.trim() {
                    field.update(cx, |field, cx| field.set_value(String::new(), cx));
                    self.restored_drafts.remove(&agent_id);
                }
            } else if self
                .restored_drafts
                .get(&agent_id)
                .is_some_and(|draft| draft.trim() == accepted.trim())
            {
                self.restored_drafts.remove(&agent_id);
            }
        }
        self.capture_drafts(cx);
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
                    if let Some(prompt) = conversation.unsent() {
                        self.accepted_drafts.insert(agent_id, prompt.text.clone());
                    }
                    conversation.clear_unsent();
                }
                if let Some(agent_id) = agent_id {
                    self.show_prompt_title(agent_id);
                }
                // The composer is synchronized before its accepted text is written again.
                should_save = false;
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
            SessionUpdate::Finished { outcome } => {
                if let Some(agent_id) = agent_id {
                    self.mark_native_local_turn(agent_id);
                    self.runners.remove(&agent_id);
                    self.pending_interactions.remove(&agent_id);
                    if outcome.is_completed() {
                        self.start_auto_title(agent_id);
                    }
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

    pub fn next_interaction(&self) -> Option<(Uuid, fx_agent::ProviderEvent)> {
        if let Some(id) = self.snapshot.active_agent_id
            && let Some(event) = self
                .pending_interactions
                .get(&id)
                .and_then(|queue| queue.front())
        {
            return Some((id, event.clone()));
        }
        self.pending_interactions
            .iter()
            .find_map(|(&id, queue)| queue.front().map(|event| (id, event.clone())))
    }

    pub fn respond_to_interaction(
        &self,
        agent_id: Uuid,
        response: fx_agent::provider::UserResponse,
    ) -> bool {
        self.runners
            .get(&agent_id)
            .is_some_and(|runner| runner.respond(response))
    }

    /// The conversation for a thread, if it has been loaded.
    pub fn conversation_by_agent(&self, agent_id: Uuid) -> Option<Arc<GuardedConversation>> {
        self.conversations.get(&agent_id).cloned()
    }

    /// Clear a thread's draft after its prompt is sent.
    pub fn clear_draft(&mut self, agent_id: Uuid, cx: &mut App) {
        if let Some(field) = self.drafts.get(&agent_id).cloned() {
            field.update(cx, |field, cx| field.set_value(String::new(), cx));
        }
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

    /// Persist only loaded conversations. Unopened and unreadable siblings remain untouched.
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
            let (mut persisted, version) = {
                let conversation = conversation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let version = (conversation.revision(), draft.clone());
                if self.saved_conversation_versions.get(&agent_id) == Some(&version) {
                    continue;
                }
                (persisted_from(&conversation), version)
            };
            persisted.total_cost_usd = self
                .restored_costs
                .get(&agent_id)
                .copied()
                .unwrap_or_default();
            persisted.draft_text = draft;
            if self
                .accepted_drafts
                .get(&agent_id)
                .is_some_and(|accepted| persisted.draft_text.trim() == accepted.trim())
            {
                persisted.draft_text.clear();
            }
            let Ok(mut bytes) = serde_json::to_vec(&persisted) else {
                continue;
            };
            if bytes.len() as u64 > fx_store::format::MAX_CONVERSATION_BYTES {
                // A full native transcript can exceed the cache limit. Its
                // draft is local-only and must still survive quitting.
                match self.store.load_conversation(project_id, agent_id) {
                    Ok(previous) => {
                        match encode_partial_cache(
                            persisted,
                            previous,
                            fx_store::format::MAX_CONVERSATION_BYTES,
                        ) {
                            Ok((record, encoded)) => {
                                persisted = record;
                                bytes = encoded;
                            }
                            Err(error) => {
                                self.last_error = Some(error);
                                continue;
                            }
                        }
                        self.last_error = Some("This conversation exceeds the local history cache limit. The complete history remains with the provider and in memory; its draft and session have been saved with the available cached history.".into());
                    }
                    Err(error) => {
                        self.last_error = Some(format!(
                            "Could not preserve this large conversation's draft without replacing an unreadable cache: {error}"
                        ));
                        continue;
                    }
                }
            }
            if self.saved_conversations.get(&agent_id) == Some(&bytes) {
                self.saved_conversation_versions.insert(agent_id, version);
                continue;
            }
            let result = if persisted.is_worth_persisting() {
                self.store
                    .save_conversation(project_id, agent_id, &persisted)
                    .map_err(|error| error.to_string())
            } else {
                self.store
                    .delete_conversation(project_id, agent_id)
                    .map_err(|error| error.to_string())
            };
            match result {
                Ok(()) => {
                    self.saved_conversations.insert(agent_id, bytes);
                    self.saved_conversation_versions.insert(agent_id, version);
                }
                Err(error) => {
                    tracing::error!(%agent_id, %error, "could not save the conversation");
                    self.last_error = Some(format!("Could not save the conversation: {error}"));
                }
            }
        }
    }

    /// Throttled recovery checkpoints while a turn streams or a draft changes.
    pub fn checkpoint(&mut self, cx: &App) {
        if self.last_checkpoint.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_checkpoint = Instant::now();
        self.capture_drafts(cx);
        self.save_conversations();
    }

    /// Save the whole loaded workspace, including drafts and partial streamed text, on quit.
    pub fn save_all(&mut self, cx: &App) {
        self.capture_drafts(cx);
        self.save_conversations();
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
        let mut changed = false;
        for completion in self.native_sync.drain() {
            self.capture_drafts(cx);
            self.apply_native_completion(completion);
            changed = true;
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
                let conversation = self.conversation(project, agent);
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
                    Ok(()) => changed = true,
                    Err(error) => {
                        self.native_sync
                            .warnings
                            .insert((project, provider.into()), vec![error]);
                        changed = true;
                    }
                }
            }
        }
        changed
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

/// Keep local-only drafts and resume metadata even when native history cannot
/// fit the bounded recovery cache. The caller reports this partial cache.
fn encode_partial_cache(
    mut record: PersistedConversation,
    previous: Option<PersistedConversation>,
    limit: u64,
) -> Result<(PersistedConversation, Vec<u8>), String> {
    record.messages = previous.map(|old| old.messages).unwrap_or_default();
    let mut bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        record.messages.clear();
        bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
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
                state.preferences.auto_titles.codex_model_id = "lightweight-title-model".into();
                state.show_prompt_title(id);
                assert_eq!(
                    state.thread(id).unwrap().title,
                    "Fix sidebar alignment and resize behavior"
                );
                let request = state.prepare_auto_title(id).unwrap();
                assert_eq!(request.provider, "codex");
                assert_eq!(request.model, "lightweight-title-model");
                assert_eq!(request.assistant, "Aligned the rows and added resizing.");
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
            "provider-change",
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
                        "provider-change" => {
                            state.snapshot.projects[0].agents[0]
                                .configuration
                                .provider_id = Some("claude".into())
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
    fn first_successful_exchange_ignores_prior_failed_or_cancelled_turns() {
        use crate::session::SessionUpdate;
        use fx_agent::codex::TurnOutcome;

        for cancelled in [false, true] {
            for partial_response in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let test = gpui::TestAppContext::single();
                let (state, id) = test.update(|cx| {
                    new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path())
                });
                test.update(|cx| {
                    state.update(cx, |state, _| {
                        let mut messages = vec![ConversationMessage::text(
                            MessageRole::User,
                            "Abandoned first request",
                        )];
                        if partial_response {
                            messages.push(ConversationMessage::text(
                                MessageRole::Assistant,
                                "Incomplete earlier response",
                            ));
                        }
                        state.conversations[&id]
                            .lock()
                            .unwrap()
                            .replace_messages(messages);
                        state.apply_update(Some(id), SessionUpdate::Accepted);
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
                        assert!(state.auto_title_record(id).is_some());
                        {
                            let mut conversation = state.conversations[&id].lock().unwrap();
                            conversation.append(ConversationMessage::text(
                                MessageRole::User,
                                "Fix the successful request instead",
                            ));
                            conversation.append(ConversationMessage::text(
                                MessageRole::Assistant,
                                "The requested fix is complete.",
                            ));
                        }
                        state.apply_update(Some(id), SessionUpdate::Accepted);
                        assert_eq!(
                            state.thread(id).unwrap().title,
                            "Fix the successful request instead"
                        );
                        // Inspect the same preparation used on Completed, without
                        // launching a real provider or hiding which exchange it saw.
                        let request = state.prepare_auto_title(id).unwrap();
                        assert_eq!(request.user, "Fix the successful request instead");
                        assert_eq!(request.assistant, "The requested fix is complete.");
                        assert!(state.prepare_auto_title(id).is_none());
                    })
                });
            }
        }
    }

    #[test]
    fn completed_exchange_without_assistant_keeps_eligibility_for_next_completion() {
        use crate::session::SessionUpdate;
        use fx_agent::codex::TurnOutcome;

        let temp = tempfile::tempdir().unwrap();
        let test = gpui::TestAppContext::single();
        let (state, id) =
            test.update(|cx| new_chat(cx, AppPaths::at(temp.path().join("state")), temp.path()));
        test.update(|cx| {
            state.update(cx, |state, _| {
                state.conversations[&id]
                    .lock()
                    .unwrap()
                    .replace_messages(vec![ConversationMessage::text(
                        MessageRole::User,
                        "First request has no response",
                    )]);
                let completed = || SessionUpdate::Finished {
                    outcome: TurnOutcome::Completed {
                        thread_id: "test".into(),
                        stop_reason: "end_turn".into(),
                        events: vec![],
                    },
                };
                state.apply_update(Some(id), completed());
                assert!(state.auto_title_record(id).is_some());
                assert_eq!(
                    state.store.load_app().unwrap().projects[0].auto_title_pending,
                    vec![id]
                );
                {
                    let mut conversation = state.conversations[&id].lock().unwrap();
                    conversation.append(ConversationMessage::text(
                        MessageRole::User,
                        "The next request",
                    ));
                    conversation.append(ConversationMessage::text(
                        MessageRole::Assistant,
                        "A complete response",
                    ));
                }
                state.apply_update(Some(id), completed());
                assert!(state.auto_title_record(id).is_none());
                assert!(
                    state.store.load_app().unwrap().projects[0]
                        .auto_title_pending
                        .is_empty()
                );
            })
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
                let conversation = state.active_conversation().unwrap();
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
                state.accepted_drafts.insert(second, "accepted".into());
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
                assert!(!state.accepted_drafts.contains_key(&second));
                assert!(!state.saved_conversations.contains_key(&second));
                assert!(!state.restored_costs.contains_key(&second));
                assert!(state.next_interaction().is_none());
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
                assert!(state.next_interaction().is_none());
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
                assert!(state.prepare_prompt(agent, field.read(cx).value(), cx));
                let update = state.update_sender_for(agent);
                update(SessionUpdate::Failed {
                    reason: "CLI unavailable".into(),
                });
                state.drain_updates_with_context(cx);
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
                assert_eq!(state.next_interaction().unwrap().0, second);
                second_updates(SessionUpdate::InteractionResolved {
                    id: "second-question".into(),
                });
                state.drain_updates();
                assert_eq!(state.next_interaction().unwrap().0, first);
                first_updates(SessionUpdate::Failed {
                    reason: "process exited".into(),
                });
                state.drain_updates();
                assert!(state.next_interaction().is_none());
                assert!(!state.respond_to_interaction(
                    first,
                    fx_agent::provider::UserResponse::CancelQuestions {
                        id: "first-question".into()
                    }
                ));
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
