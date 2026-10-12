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
        write::write_json_atomically(&self.paths.projects_file(), app)?;
        Ok(())
    }

    pub fn load_preferences(&self) -> Result<PersistedPreferences, LoadError> {
        let path = self.paths.preferences_file();
        if !path.exists() && !write::has_backup(&path) {
            return Ok(PersistedPreferences::default());
        }
        read_bounded(&path, crate::format::MAX_PROJECTS_BYTES)
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

    /// Remove one thread's transcript.
    pub fn delete_conversation(&self, project_id: Uuid, agent_id: Uuid) -> std::io::Result<()> {
        write::remove(&self.paths.conversation_file(project_id, agent_id))
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
        let preferences = PersistedPreferences {
            appearance: "light".into(),
            accent: "orange".into(),
            ..Default::default()
        };
        store.save_preferences(&preferences).unwrap();
        assert_eq!(store.load_preferences().unwrap(), preferences);
        std::fs::write(store.paths().preferences_file(), b"{ damaged").unwrap();
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
