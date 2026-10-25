//! The store the app actually calls.
//!
//! One type, four operations. Everything about *where* files live and *how* they
//! are written is behind it, so the app never touches a path.

use std::collections::BTreeMap;

use fx_core::Conversation;
use uuid::Uuid;

use crate::format::{PersistedApp, PersistedConversation, PersistedPreferences};
use crate::paths::AppPaths;
use crate::write::{self, ReadError, WriteError};

/// What happened while loading.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error("the file is larger than the {limit} byte limit")]
    TooLarge { limit: u64 },
}

/// What happened while saving.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(
        "the workspace exceeds its {limit} byte metadata limit; the previous saved workspace has been preserved"
    )]
    TooLarge { limit: u64 },
}

/// Persistent storage for one FlowX installation.
#[derive(Debug, Clone)]
pub struct Store {
    paths: AppPaths,
}

impl Store {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// Read the project snapshot, or an empty one on first run.
    ///
    /// A missing file is the normal first-run state, not an error. A file that
    /// is present but unreadable is reported, because silently starting empty
    /// would look identical to losing every project.
    pub fn load_app(&self) -> Result<PersistedApp, LoadError> {
        let path = self.paths.projects_file();
        if !path.exists() && !write::has_backup(&path) {
            return Ok(PersistedApp::default());
        }
        let mut app: PersistedApp = read_bounded(&path, crate::format::MAX_PROJECTS_BYTES)?;
        // Repair on read rather than waiting for the next write: a file that
        // cannot be opened is one that also cannot be saved back.
        app.normalize();
        app.drop_unusable_projects();
        Ok(app)
    }

    pub fn save_app(&self, app: &PersistedApp) -> Result<(), SaveError> {
        // Bound the encoding that will actually be read back. Compact JSON can
        // fit while this pretty encoding exceeds the reader's limit, making
        // both the primary and its recovery copy unreadable after a good save.
        let bytes = serde_json::to_vec_pretty(app).map_err(WriteError::from)?;
        let limit = crate::format::MAX_PROJECTS_BYTES;
        if bytes.len() as u64 > limit {
            return Err(SaveError::TooLarge { limit });
        }
        write::write_atomically(&self.paths.projects_file(), &bytes)?;
        Ok(())
    }

    pub fn load_preferences(&self) -> Result<PersistedPreferences, LoadError> {
        let path = self.paths.preferences_file();
        if !path.exists() && !write::has_backup(&path) {
            return Ok(PersistedPreferences::default());
        }
        read_bounded(&path, crate::format::MAX_PROJECTS_BYTES)
    }

    /// Read only the committed primary file. Unlike normal recovery loading,
    /// neither a missing primary nor its backup can confirm a partial save.
    pub fn load_preferences_primary(&self) -> Result<PersistedPreferences, LoadError> {
        use std::io::Read;

        let limit = crate::format::MAX_PROJECTS_BYTES;
        let file = std::fs::File::open(self.paths.preferences_file())?;
        if file.metadata()?.len() > limit {
            return Err(LoadError::TooLarge { limit });
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(LoadError::TooLarge { limit });
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn save_preferences(&self, preferences: &PersistedPreferences) -> Result<(), SaveError> {
        write::write_json_atomically(&self.paths.preferences_file(), preferences)?;
        Ok(())
    }

    /// Read one thread, or `None` when it has never been saved.
    pub fn load_conversation(
        &self,
        project_id: Uuid,
        agent_id: Uuid,
    ) -> Result<Option<PersistedConversation>, LoadError> {
        let path = self.paths.conversation_file(project_id, agent_id);
        if !path.exists() && !write::has_backup(&path) {
            return Ok(None);
        }
        read_bounded(&path, crate::format::MAX_CONVERSATION_BYTES).map(Some)
    }

    pub fn save_conversation(
        &self,
        project_id: Uuid,
        agent_id: Uuid,
        conversation: &PersistedConversation,
    ) -> Result<(), SaveError> {
        write::write_json_atomically(
            &self.paths.conversation_file(project_id, agent_id),
            conversation,
        )?;
        Ok(())
    }

    /// Write a transcript that was encoded with [`EncodedConversation::new`].
    pub fn save_encoded_conversation(
        &self,
        project_id: Uuid,
        agent_id: Uuid,
        encoded: &EncodedConversation,
    ) -> Result<(), SaveError> {
        write::write_atomically(&self.paths.conversation_file(project_id, agent_id), encoded)?;
        Ok(())
    }

    /// Whether a thread has a transcript or recovery copy, without reading it.
    pub fn has_conversation(&self, project_id: Uuid, agent_id: Uuid) -> bool {
        let path = self.paths.conversation_file(project_id, agent_id);
        path.exists() || write::has_backup(&path)
    }

    /// Remove one thread's transcript.
    pub fn delete_conversation(&self, project_id: Uuid, agent_id: Uuid) -> std::io::Result<()> {
        write::remove(&self.paths.conversation_file(project_id, agent_id))
    }

    /// Delete the local transcript, recovery copy, and attachment cache only.
    /// The caller commits removal from the snapshot first, so a cleanup error
    /// can leave recoverable orphan files but never resurrect the sidebar row.
    pub fn delete_thread_files(&self, project_id: Uuid, agent_id: Uuid) -> std::io::Result<()> {
        self.delete_conversation(project_id, agent_id)?;
        let assets = self.paths.assets_dir(project_id, agent_id);
        match std::fs::symlink_metadata(&assets) {
            Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(assets),
            Ok(_) => std::fs::remove_file(assets),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Read every saved thread for a project, keyed by agent.
    ///
    /// One unreadable thread is skipped rather than failing the whole project:
    /// a corrupt file for thread A must not make thread B unreachable.
    pub fn load_project_conversations(
        &self,
        project_id: Uuid,
    ) -> BTreeMap<Uuid, PersistedConversation> {
        let mut out = BTreeMap::new();
        for entry in self.conversation_entries(project_id) {
            let Ok(Some(conversation)) = self.load_conversation(project_id, entry) else {
                tracing::warn!(%entry, "a saved transcript could not be read; skipping it");
                continue;
            };
            out.insert(entry, conversation);
        }
        out
    }

    /// Which threads have a transcript file for a project.
    pub fn conversation_entries(&self, project_id: Uuid) -> Vec<Uuid> {
        let dir = self.paths.conversation_file(project_id, Uuid::nil());
        let Ok(entries) = std::fs::read_dir(dir.parent().unwrap_or(&dir)) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?;
                let stem = name
                    .strip_suffix(".json")
                    .or_else(|| name.strip_suffix(".json.backup"))?;
                stem.parse().ok()
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Save loaded transcripts worth keeping, and delete explicitly emptied ones.
    ///
    /// An emptied thread is removed rather than written empty, so quitting does
    /// not leave a stub file behind for every thread ever opened. A missing
    /// entry means the transcript is unloaded, never that it should be deleted.
    pub fn prune_conversations(
        &self,
        project_id: Uuid,
        live: &BTreeMap<Uuid, Conversation>,
    ) -> Result<Vec<Uuid>, SaveError> {
        let mut removed = Vec::new();
        for (&entry, conversation) in live {
            let mut persisted = match self.load_conversation(project_id, entry) {
                Ok(Some(persisted)) => persisted,
                Ok(None) => PersistedConversation::from_messages(entry, Vec::new()),
                // Never replace a file whose contents are not known.
                Err(error) => {
                    tracing::warn!(%entry, %error, "preserving an unreadable transcript");
                    continue;
                }
            };
            persisted.messages.clone_from(&conversation.messages);
            if persisted.is_worth_persisting() {
                self.save_conversation(project_id, entry, &persisted)?;
            } else if self.delete_conversation(project_id, entry).is_ok() {
                removed.push(entry);
            }
        }
        Ok(removed)
    }
}

/// A transcript in its on-disk encoding.
///
/// Encoded once so a caller can measure it against
/// [`crate::format::MAX_CONVERSATION_BYTES`] and compare it with what it last
/// wrote before deciding to write it. Measure these bytes, not a more compact
/// encoding: a later read is bounded by the file as written, so a smaller
/// measure would let through a file that could not be read back.
pub struct EncodedConversation(Vec<u8>);

impl EncodedConversation {
    pub fn new(conversation: &PersistedConversation) -> Result<Self, serde_json::Error> {
        Ok(Self(serde_json::to_vec_pretty(conversation)?))
    }
}

impl std::ops::Deref for EncodedConversation {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

fn read_bounded<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    limit: u64,
) -> Result<T, LoadError> {
    use std::io::Read;

    let mut too_large = false;
    for candidate in [path.to_path_buf(), write::backup_path(path)] {
        let Ok(file) = std::fs::File::open(&candidate) else {
            continue;
        };
        if file.metadata()?.len() > limit {
            too_large = true;
            continue;
        }
        let mut bytes = Vec::new();
        // A file may grow between metadata and read. Bound the read itself too.
        if file.take(limit + 1).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if bytes.len() as u64 > limit {
            too_large = true;
            continue;
        }
        if let Ok(value) = serde_json::from_slice(&bytes) {
            return Ok(value);
        }
    }
    if too_large {
        Err(LoadError::TooLarge { limit })
    } else {
        Err(ReadError::Unreadable {
            path: path.display().to_string(),
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{PersistedProject, PersistedWorkspace, RightPanelTab};
    use fx_core::{Conversation, ConversationMessage, MessageRole, Project};

    fn store() -> (Store, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(AppPaths::at(temp.path().join("state")));
        store.paths().ensure().unwrap();
        (store, temp)
    }

    /// A user message, so tests do not each repeat the constructor.
    fn message(text: &str) -> ConversationMessage {
        ConversationMessage::text(MessageRole::User, text)
    }

    fn populated() -> PersistedApp {
        let path = tempfile::tempdir().unwrap();
        let path = path.path().to_string_lossy().into_owned();
        PersistedApp {
            projects: vec![PersistedProject {
                project: Some(Project::new(&path)),
                workspaces: vec![PersistedWorkspace::default()],
                ..Default::default()
            }],
            right_panel_tab: RightPanelTab::Changes,
            ..Default::default()
        }
    }

    #[test]
    fn a_fresh_installation_loads_an_empty_snapshot() {
        let (store, _temp) = store();
        let app = store.load_app().unwrap();
        assert!(app.projects.is_empty());
    }

    #[test]
    fn a_snapshot_round_trips_through_the_store() {
        let (store, _temp) = store();
        store.save_app(&populated()).unwrap();
        let loaded = store.load_app().unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.right_panel_tab, RightPanelTab::Changes);
    }

    #[test]
    fn old_mode_values_are_removed_on_save_without_losing_thread_configuration_or_history() {
        let (store, _temp) = store();
        let mut app = populated();
        let project_id = app.projects[0].project.as_ref().unwrap().id;
        let mut agent = fx_core::Agent::new("Keep this conversation");
        agent.configuration = fx_core::AgentConfiguration {
            provider_id: Some("codex".into()),
            model_id: Some("saved-model".into()),
            effort: Some("high".into()),
            system_prompt: Some("Saved instructions".into()),
            agent_access: Some(fx_core::AgentAccess::Supervised),
            context_window_size: Some(200_000),
        };
        let agent_id = agent.id;
        app.projects[0].agents.push(agent.clone());
        app.active_project_id = Some(project_id);
        app.active_agent_id = Some(agent_id);
        let mut legacy = serde_json::to_value(&app).unwrap();
        legacy["projects"][0]["agents"][0]["configuration"]["agent_mode"] =
            serde_json::json!("plan");
        legacy["projects"][0]["agents"][0]["configuration"]["agentMode"] =
            serde_json::json!("auto");
        legacy["projects"][0]["native_bindings"] = serde_json::json!([{
            "agentId": agent_id,
            "providerId": "codex",
            "sessionId": "saved-native-session",
            "title": "Keep this conversation",
            "workingDirectory": app.projects[0].project.as_ref().unwrap().root_path,
            "updatedAt": agent.created_at,
            "model": "saved-model",
            "effort": "high",
            "agentMode": "plan",
            "agentAccess": "supervised"
        }]);
        store.paths().ensure().unwrap();
        std::fs::write(
            store.paths().projects_file(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let mut conversation =
            PersistedConversation::from_messages(agent_id, vec![message("Keep the transcript")]);
        conversation.session_id = Some("saved-native-session".into());
        conversation.draft_text = "Keep the draft".into();
        store
            .save_conversation(project_id, agent_id, &conversation)
            .unwrap();

        let restored = store.load_app().unwrap();
        assert_eq!(restored.projects[0].agents, vec![agent.clone()]);
        let binding = &restored.projects[0].native_bindings[0];
        assert_eq!(binding.session_id, "saved-native-session");
        assert_eq!(binding.model.as_deref(), Some("saved-model"));
        assert_eq!(binding.effort.as_deref(), Some("high"));
        assert_eq!(binding.agent_access, Some(fx_core::AgentAccess::Supervised));
        store.save_app(&restored).unwrap();
        let rewritten = std::fs::read_to_string(store.paths().projects_file()).unwrap();
        assert!(!rewritten.contains("agentMode"));
        assert!(!rewritten.contains("agent_mode"));
        let reopened = store.load_app().unwrap();
        assert_eq!(reopened.projects[0].agents, vec![agent]);
        assert_eq!(reopened.active_agent_id, Some(agent_id));
        let reopened_history = store
            .load_conversation(project_id, agent_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(reopened_history).unwrap(),
            serde_json::to_value(conversation).unwrap()
        );
    }

    #[test]
    fn a_newer_builds_entries_survive_a_load_and_save_without_hiding_the_rest() {
        let (store, _temp) = store();
        let mut app = populated();
        let thread = fx_core::Agent::new("Readable");
        app.projects[0].agents.push(thread.clone());
        let mut file = serde_json::to_value(&app).unwrap();
        file["projects"][0]["agents"][0]["execution_state"] = serde_json::json!("cancelled");
        let foreign_thread = serde_json::json!({"id": "from-a-newer-build", "title": "Kept"});
        let foreign_project = serde_json::json!({"project": {"rootPath": 42}});
        file["projects"][0]["agents"]
            .as_array_mut()
            .unwrap()
            .push(foreign_thread.clone());
        file["projects"]
            .as_array_mut()
            .unwrap()
            .push(foreign_project.clone());
        // Both copies hold the same bytes, as they do after any save.
        let bytes = serde_json::to_vec(&file).unwrap();
        std::fs::write(store.paths().projects_file(), &bytes).unwrap();
        std::fs::write(write::backup_path(&store.paths().projects_file()), &bytes).unwrap();

        let loaded = store.load_app().unwrap();
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].agents.len(), 1);
        assert_eq!(loaded.projects[0].agents[0].id, thread.id);

        store.save_app(&loaded).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.paths().projects_file()).unwrap()).unwrap();
        assert_eq!(saved["projects"][1], foreign_project);
        assert_eq!(saved["projects"][0]["agents"][1], foreign_thread);
        assert_eq!(saved["projects"][0]["agents"][0]["execution_state"], "idle");
    }

    #[test]
    fn a_snapshot_is_repaired_on_read() {
        // A geometry value that could not be opened is repaired before it is
        // ever used, so it is not just repaired on the next write.
        let (store, _temp) = store();
        let mut app = populated();
        app.right_panel_width = 99_999.0;
        store.save_app(&app).unwrap();

        let loaded = store.load_app().unwrap();
        assert_eq!(loaded.right_panel_width, app.max_right_panel_width);
    }

    #[test]
    fn an_unreadable_snapshot_is_reported_rather_than_read_as_empty() {
        // Starting empty would be indistinguishable from losing every project.
        let (store, _temp) = store();
        store.paths().ensure().unwrap();
        std::fs::write(store.paths().projects_file(), b"{ not json").unwrap();
        std::fs::write(
            write::backup_path(&store.paths().projects_file()),
            b"{ also not",
        )
        .unwrap();
        assert!(store.load_app().is_err());
    }

    #[test]
    fn an_absent_transcript_reads_as_absent_not_as_an_error() {
        let (store, _temp) = store();
        let id = Uuid::from_u128(5);
        assert!(store.load_conversation(id, id).unwrap().is_none());
    }

    #[test]
    fn a_transcript_round_trips() {
        let (store, _temp) = store();
        let (project, agent) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let mut conversation = Conversation::new(agent);
        conversation.push(message("hello"));

        store
            .save_conversation(
                project,
                agent,
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        let loaded = store.load_conversation(project, agent).unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].text_content(), "hello");
    }

    #[test]
    fn an_encoded_transcript_is_written_as_measured_and_seen_without_a_read() {
        let (store, _temp) = store();
        let (project, agent) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert!(!store.has_conversation(project, agent));
        let encoded = EncodedConversation::new(&PersistedConversation::from_messages(
            agent,
            vec![message("hello")],
        ))
        .unwrap();
        store
            .save_encoded_conversation(project, agent, &encoded)
            .unwrap();
        let path = store.paths().conversation_file(project, agent);
        // The limit is checked against these bytes, so they must be the file.
        assert_eq!(std::fs::read(&path).unwrap(), &*encoded);
        assert_eq!(std::fs::read(write::backup_path(&path)).unwrap(), &*encoded);
        assert!(store.has_conversation(project, agent));
        std::fs::remove_file(&path).unwrap();
        assert!(
            store.has_conversation(project, agent),
            "the backup still counts"
        );
        assert_eq!(
            store
                .load_conversation(project, agent)
                .unwrap()
                .unwrap()
                .messages[0]
                .text_content(),
            "hello"
        );
    }

    #[test]
    fn transcripts_are_listed_per_project() {
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let other = Uuid::from_u128(9);
        for agent in [Uuid::from_u128(1), Uuid::from_u128(2)] {
            let mut conversation = Conversation::new(agent);
            conversation.push(ConversationMessage::text(MessageRole::User, "x"));
            store
                .save_conversation(
                    project,
                    agent,
                    &PersistedConversation::from_messages(
                        conversation.agent_id,
                        conversation.messages.clone(),
                    ),
                )
                .unwrap();
        }
        let mut conversation = Conversation::new(Uuid::from_u128(3));
        conversation.push(ConversationMessage::text(MessageRole::User, "x"));
        store
            .save_conversation(
                other,
                Uuid::from_u128(3),
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        assert_eq!(store.conversation_entries(project).len(), 2);
        assert_eq!(store.conversation_entries(other).len(), 1);
    }

    #[test]
    fn one_corrupt_transcript_does_not_hide_the_others() {
        // The failure mode this guards against: a bad file for thread A making
        // thread B unreachable.
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let good = Uuid::from_u128(1);
        let bad = Uuid::from_u128(2);

        let mut conversation = Conversation::new(good);
        conversation.push(ConversationMessage::text(MessageRole::User, "x"));
        store
            .save_conversation(
                project,
                good,
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        let bad_path = store.paths().conversation_file(project, bad);
        std::fs::create_dir_all(bad_path.parent().unwrap()).unwrap();
        std::fs::write(&bad_path, b"{ truncated").unwrap();
        std::fs::write(write::backup_path(&bad_path), b"{ also truncated").unwrap();

        let loaded = store.load_project_conversations(project);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&good));
    }

    #[test]
    fn pruning_preserves_a_thread_that_has_not_been_loaded() {
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let gone = Uuid::from_u128(1);
        let mut conversation = Conversation::new(gone);
        conversation.push(ConversationMessage::text(MessageRole::User, "x"));
        store
            .save_conversation(
                project,
                gone,
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        let removed = store
            .prune_conversations(project, &BTreeMap::new())
            .unwrap();
        assert!(removed.is_empty());
        assert!(store.load_conversation(project, gone).unwrap().is_some());
    }

    #[test]
    fn a_missing_primary_restores_its_bounded_backup() {
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let agent = Uuid::from_u128(2);
        store.save_app(&PersistedApp::default()).unwrap();
        std::fs::remove_file(store.paths().projects_file()).unwrap();
        assert!(store.load_app().is_ok());
        store
            .save_conversation(
                project,
                agent,
                &PersistedConversation::from_messages(agent, vec![message("recovered")]),
            )
            .unwrap();
        std::fs::remove_file(store.paths().conversation_file(project, agent)).unwrap();
        assert_eq!(store.conversation_entries(project), vec![agent]);
        assert_eq!(
            store
                .load_conversation(project, agent)
                .unwrap()
                .unwrap()
                .messages[0]
                .text_content(),
            "recovered"
        );
    }

    #[test]
    fn an_oversized_primary_can_still_use_a_small_recovery_copy() {
        let (store, _temp) = store();
        store.save_app(&PersistedApp::default()).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(store.paths().projects_file())
            .unwrap()
            .set_len(crate::format::MAX_PROJECTS_BYTES + 1)
            .unwrap();
        assert!(store.load_app().is_ok());
    }

    #[test]
    fn an_oversized_written_encoding_preserves_both_saved_snapshots() {
        let (store, _temp) = store();
        let previous = populated();
        store.save_app(&previous).unwrap();
        let path = store.paths().projects_file();
        let backup = write::backup_path(&path);
        let primary_before = std::fs::read(&path).unwrap();
        let backup_before = std::fs::read(&backup).unwrap();

        let mut next = previous.clone();
        let limit = crate::format::MAX_PROJECTS_BYTES;
        let compact_size = serde_json::to_vec(&next).unwrap().len();
        next.projects[0].project.as_mut().unwrap().description =
            "x".repeat(limit as usize - compact_size);
        assert_eq!(serde_json::to_vec(&next).unwrap().len() as u64, limit);
        assert!(serde_json::to_vec_pretty(&next).unwrap().len() as u64 > limit);

        assert!(matches!(
            store.save_app(&next),
            Err(SaveError::TooLarge { limit: actual }) if actual == limit
        ));
        assert_eq!(std::fs::read(&path).unwrap(), primary_before);
        assert_eq!(std::fs::read(&backup).unwrap(), backup_before);
        assert_eq!(store.load_app().unwrap(), previous);
    }

    #[test]
    fn the_backup_cannot_bypass_the_size_limit() {
        let (store, _temp) = store();
        let path = store.paths().projects_file();
        std::fs::write(&path, b"bad json").unwrap();
        std::fs::File::create(write::backup_path(&path))
            .unwrap()
            .set_len(crate::format::MAX_PROJECTS_BYTES + 1)
            .unwrap();
        assert!(matches!(store.load_app(), Err(LoadError::TooLarge { .. })));
    }

    #[test]
    fn preferences_round_trip_and_recover_without_ui_dependencies() {
        let (store, _temp) = store();
        assert_eq!(
            store.load_preferences().unwrap(),
            PersistedPreferences::default()
        );
        assert!(store.load_preferences_primary().is_err());
        let preferences = PersistedPreferences {
            appearance: "light".into(),
            accent: "orange".into(),
            codex_defaults: crate::ProviderDefaults {
                model_id: Some("codex-model".into()),
                effort: Some("high".into()),
                agent_access: Some(fx_core::AgentAccess::Supervised),
            },
            claude_defaults: crate::ProviderDefaults {
                model_id: Some("claude-model".into()),
                effort: Some("low".into()),
                agent_access: Some(fx_core::AgentAccess::AcceptEdits),
            },
            ..Default::default()
        };
        store.save_preferences(&preferences).unwrap();
        assert_eq!(store.load_preferences().unwrap(), preferences);
        assert_eq!(store.load_preferences_primary().unwrap(), preferences);
        std::fs::write(store.paths().preferences_file(), b"{ damaged").unwrap();
        assert!(store.load_preferences_primary().is_err());
        assert_eq!(store.load_preferences().unwrap(), preferences);
    }

    #[test]
    fn pruning_saves_a_live_thread() {
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let agent = Uuid::from_u128(1);
        let mut conversation = Conversation::new(agent);
        conversation.push(ConversationMessage::text(MessageRole::User, "kept"));
        store
            .save_conversation(
                project,
                agent,
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        let mut live = BTreeMap::new();
        live.insert(agent, conversation);
        store.prune_conversations(project, &live).unwrap();

        assert_eq!(
            store
                .load_conversation(project, agent)
                .unwrap()
                .unwrap()
                .messages[0]
                .text_content(),
            "kept"
        );
    }

    #[test]
    fn pruning_removes_an_emptied_thread_rather_than_writing_a_stub() {
        let (store, _temp) = store();
        let project = Uuid::from_u128(1);
        let agent = Uuid::from_u128(1);
        let mut conversation = Conversation::new(agent);
        conversation.push(message("x"));
        store
            .save_conversation(
                project,
                agent,
                &PersistedConversation::from_messages(
                    conversation.agent_id,
                    conversation.messages.clone(),
                ),
            )
            .unwrap();

        let mut live = BTreeMap::new();
        live.insert(agent, Conversation::new(agent));
        store.prune_conversations(project, &live).unwrap();

        assert!(store.conversation_entries(project).is_empty());
    }

    #[test]
    fn an_oversized_snapshot_is_refused() {
        // A size check stops a runaway file from being read into memory. The
        // file is sparse, so the test costs no disk.
        let (store, _temp) = store();
        store.paths().ensure().unwrap();
        let path = store.paths().projects_file();
        oversize(&path, crate::format::MAX_PROJECTS_BYTES);

        assert!(matches!(store.load_app(), Err(LoadError::TooLarge { .. })));
    }

    #[test]
    fn an_oversized_transcript_is_refused() {
        let (store, _temp) = store();
        let (project, agent) = (Uuid::from_u128(1), Uuid::from_u128(2));
        oversize(
            &store.paths().conversation_file(project, agent),
            crate::format::MAX_CONVERSATION_BYTES,
        );

        assert!(matches!(
            store.load_conversation(project, agent),
            Err(LoadError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_file_just_under_the_limit_is_still_read() {
        // The limit must not refuse a large-but-plausible file.
        let (store, _temp) = store();
        store.paths().ensure().unwrap();
        let path = store.paths().projects_file();
        let app = crate::format::PersistedApp::default();
        write::write_json_atomically(&path, &app).unwrap();
        assert!(store.load_app().is_ok());
    }

    /// A sparse file one byte past a limit, for the file and its recovery copy.
    fn oversize(path: &std::path::Path, limit: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for target in [path.to_path_buf(), write::backup_path(path)] {
            let file = std::fs::File::create(&target).unwrap();
            file.set_len(limit + 1).unwrap();
        }
    }
}
