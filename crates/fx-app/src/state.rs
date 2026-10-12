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
use fx_core::{Agent, Project};
use fx_design::components::{ContextRegistry, MenuLayer};
use fx_design::theme::{
    AccentColor, AppearanceMode, BaseTone, Layout, TextSizePreset, Theme, global,
};
use fx_store::{AppPaths, PersistedApp, PersistedConversation, RightPanelTab, Store};

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
            restored_costs: HashMap::new(),
            last_checkpoint: Instant::now(),
            pending_interactions: HashMap::new(),
            picker: None,
            sidebar_visible,
            last_error,
            paths,
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
        let agent = fx_core::Agent::new("Untitled thread");

        let record = self
            .snapshot
            .projects
            .iter_mut()
            .find(|record| record.project.as_ref().is_some_and(|p| p.id == project_id))?;
        let project = record.project.as_mut()?;
        project.agent_order.push(agent.id);
        record.agents.push(agent.clone());
        record.is_expanded = true;

        self.conversations
            .insert(agent.id, Arc::new(Mutex::new(Conversation::new(agent.id))));
        self.snapshot.active_agent_id = Some(agent.id);
        Some(agent.id)
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
            .filter(|id| !record.archived_agent_ids.contains(id))
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
                for binding in &mut record.native_bindings {
                    if binding.agent_id == agent_id {
                        binding.title = title.to_string();
                    }
                }
                break;
            }
        }
        self.commit_thread_change(next, "rename this thread")
    }

    pub fn set_thread_archived(&mut self, agent_id: Uuid, archived: bool) -> bool {
        if !self.thread_can_change(agent_id) {
            return false;
        }
        if self.is_thread_archived(agent_id) == archived {
            return true;
        }
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if !record.agents.iter().any(|agent| agent.id == agent_id) {
                continue;
            }
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
        let mut next = self.snapshot.clone();
        for record in &mut next.projects {
            if !record.agents.iter().any(|agent| agent.id == agent_id) {
                continue;
            }
            record.agents.retain(|agent| agent.id != agent_id);
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
        self.conversations.remove(&agent_id);
        self.drafts.remove(&agent_id);
        self.runners.remove(&agent_id);
        self.protected_conversations.remove(&agent_id);
        self.restored_drafts.remove(&agent_id);
        self.accepted_drafts.remove(&agent_id);
        self.saved_conversations.remove(&agent_id);
        self.restored_costs.remove(&agent_id);
        self.pending_interactions.remove(&agent_id);
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
                    self.runners.remove(&agent_id);
                    self.pending_interactions.remove(&agent_id);
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
                    self.runners.remove(&agent_id);
                    self.pending_interactions.remove(&agent_id);
                }
                self.last_error = Some(reason);
            }
            SessionUpdate::Refused { tool, reason } => {
                tracing::info!(tool, reason, "a tool call was refused");
            }
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
            let mut persisted = persisted_from(
                &conversation
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
            persisted.total_cost_usd = self
                .restored_costs
                .get(&agent_id)
                .copied()
                .unwrap_or_default();
            persisted.draft_text = self
                .restored_drafts
                .get(&agent_id)
                .cloned()
                .unwrap_or_default();
            if self
                .accepted_drafts
                .get(&agent_id)
                .is_some_and(|accepted| persisted.draft_text.trim() == accepted.trim())
            {
                persisted.draft_text.clear();
            }
            let Ok(bytes) = serde_json::to_vec(&persisted) else {
                continue;
            };
            if self.saved_conversations.get(&agent_id) == Some(&bytes) {
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
    conversation.replace_messages(persisted.messages);
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
        !record.archived_agent_ids.contains(id) && record.agents.iter().any(|agent| agent.id == *id)
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
mod persistence_tests {
    use super::*;
    use crate::session::SessionUpdate;
    use fx_agent::provider::Usage;
    use fx_core::{ConversationMessage, MessageRole};

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
