//! Bounded native-history workers. No provider IO runs on the GPUI thread.
//!
//! The owner calls `poll_native_sync` every 500ms. Each provider has one worker
//! and one in-flight request, so an offline provider cannot block the other or
//! accumulate a backlog while a project is switched repeatedly.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use fx_agent::native::{NativeThread, NativeThreadIndex};
use fx_store::AppPaths;
use uuid::Uuid;

pub(crate) const PROVIDERS: [&str; 2] = ["codex", "claude"];
pub(crate) const INDEX_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const HISTORY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeSyncStatus {
    pub syncing: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Ticket {
    pub project: Uuid,
    pub root: String,
    pub provider: String,
    pub generation: u64,
    pub kind: RequestKind,
}

#[derive(Debug, Clone)]
pub(crate) enum RequestKind {
    Index {
        candidates: Vec<Uuid>,
        missing_candidates: Vec<(Uuid, String)>,
    },
    History {
        agent: Uuid,
        session: String,
        revision: u64,
        known_revision: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) enum Payload {
    Index {
        index: NativeThreadIndex,
        sessions: Vec<(Uuid, String)>,
        protected: Vec<Uuid>,
    },
    History(NativeThread),
    Unchanged,
}

#[derive(Debug)]
pub(crate) struct Completion {
    pub ticket: Ticket,
    pub result: Result<Payload, String>,
}

struct Worker {
    sender: SyncSender<Ticket>,
    receiver: Receiver<Completion>,
    pending: Option<Ticket>,
}

#[derive(Default)]
pub(crate) struct NativeSync {
    workers: HashMap<String, Worker>,
    pub generations: HashMap<Uuid, u64>,
    pub index_due: HashMap<(Uuid, String), Instant>,
    pub history_due: HashMap<Uuid, Instant>,
    pub warnings: HashMap<(Uuid, String), Vec<String>>,
    pub history_warnings: HashMap<Uuid, String>,
    pub selected: Option<(Uuid, Uuid)>,
    pub completed_turns: HashMap<Uuid, Instant>,
    pub last_was_index: HashMap<String, bool>,
    pub loaded_revisions: HashMap<Uuid, String>,
    #[cfg(test)]
    test_enabled: bool,
}

impl NativeSync {
    pub fn allowed(&self) -> bool {
        #[cfg(test)]
        {
            self.test_enabled
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    #[cfg(test)]
    pub fn test_worker(&mut self, provider: &str) -> (Receiver<Ticket>, SyncSender<Completion>) {
        let (sender, jobs) = sync_channel(1);
        let (results, receiver) = sync_channel(1);
        self.workers.insert(
            provider.into(),
            Worker {
                sender,
                receiver,
                pending: None,
            },
        );
        self.test_enabled = true;
        (jobs, results)
    }

    pub fn generation(&self, project: Uuid) -> u64 {
        self.generations.get(&project).copied().unwrap_or_default()
    }

    pub fn invalidate(&mut self, project: Uuid) {
        *self.generations.entry(project).or_default() += 1;
        self.index_due.retain(|(id, _), _| *id != project);
        self.warnings.retain(|(id, _), _| *id != project);
    }

    pub fn ready(&self, provider: &str) -> bool {
        self.workers
            .get(provider)
            .is_none_or(|worker| worker.pending.is_none())
    }

    pub fn pending_for(&self, project: Uuid) -> bool {
        self.workers.values().any(|worker| {
            worker
                .pending
                .as_ref()
                .is_some_and(|ticket| ticket.project == project)
        })
    }

    pub fn history_loading(&self, agent: Uuid) -> bool {
        self.workers.values().any(|worker| {
            worker.pending.as_ref().is_some_and(|ticket| {
            matches!(ticket.kind, RequestKind::History { agent: pending, .. } if pending == agent)
        })
        })
    }

    pub fn submit(&mut self, ticket: Ticket, _paths: AppPaths) -> Result<(), String> {
        let provider = ticket.provider.clone();
        if !self.workers.contains_key(&provider) {
            #[cfg(test)]
            return Err(
                "A native test backend must be explicitly installed for every provider.".into(),
            );
            #[cfg(not(test))]
            self.workers
                .insert(provider.clone(), Worker::start(&provider, _paths)?);
        }
        let worker = self.workers.get_mut(&provider).unwrap();
        worker
            .sender
            .try_send(ticket.clone())
            .map_err(|error| error.to_string())?;
        self.last_was_index
            .insert(provider, matches!(ticket.kind, RequestKind::Index { .. }));
        worker.pending = Some(ticket);
        Ok(())
    }

    pub fn drain(&mut self) -> Vec<Completion> {
        let mut out = Vec::new();
        for worker in self.workers.values_mut() {
            match worker.receiver.try_recv() {
                Ok(completion) => {
                    worker.pending = None;
                    out.push(completion);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if let Some(ticket) = worker.pending.take() {
                        out.push(Completion { ticket, result: Err("Native history worker stopped unexpectedly. Restart FlowX to reconnect.".into()) });
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        out
    }
}

#[cfg(not(test))]
impl Worker {
    fn start(provider: &str, paths: AppPaths) -> Result<Self, String> {
        let (sender, jobs) = sync_channel::<Ticket>(1);
        let (results, receiver) = sync_channel(1);
        let provider = provider.to_owned();
        std::thread::Builder::new().name(format!("native-{provider}")).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let codex = fx_agent::codex::native::CodexNativeStore::new(fx_core::services::runtime::RuntimeDiscovery::default());
            let claude = fx_agent::claude::native::ClaudeNativeStore::default();
            let store = fx_store::Store::new(paths);
            for ticket in jobs {
                let result = match &ticket.kind {
                    RequestKind::Index { candidates, missing_candidates } => {
                        let result = if provider == "codex" {
                            match &runtime {
                                Ok(runtime) => runtime.block_on(codex.list(&ticket.root)).map_err(|error| error.to_string()),
                                Err(error) => Err(error.to_string()),
                            }
                        } else {
                            claude.list(&ticket.root).map_err(|error| error.to_string())
                        };
                        result.map(|mut index| {
                            let sessions = candidates.iter().filter_map(|&agent| {
                                match store.load_conversation(ticket.project, agent) {
                                    Ok(Some(conversation)) if conversation.agent_id == agent => conversation.session_id.map(|session| (agent, session)),
                                    Ok(_) => None,
                                    Err(error) => {
                                        index.warnings.push(format!("Could not check a saved thread for duplicate native history: {error}"));
                                        None
                                    }
                                }
                            }).collect();
                            let protected = missing_candidates.iter().filter_map(|(agent, session)| {
                                if index.threads.iter().any(|entry| entry.summary.identity.session_id == *session) { return None; }
                                match store.load_conversation(ticket.project, *agent) {
                                    Ok(Some(conversation)) if !conversation.draft_text.is_empty() || conversation.unsent_prompt.is_some() => Some(*agent),
                                    Err(error) => {
                                        index.warnings.push(format!("Could not check a missing native thread's saved draft: {error}"));
                                        Some(*agent)
                                    }
                                    _ => None,
                                }
                            }).collect();
                            Payload::Index { index, sessions, protected }
                        })
                    }
                    RequestKind::History { session, known_revision, .. } => {
                        let result = if provider == "codex" {
                            match &runtime {
                                Ok(runtime) => runtime.block_on(codex.read_if_changed(&ticket.root, session, known_revision.as_deref())).map_err(|error| error.to_string()),
                                Err(error) => Err(error.to_string()),
                            }
                        } else {
                            claude.read_if_changed(&ticket.root, session, known_revision.as_deref()).map_err(|error| error.to_string())
                        };
                        result.map(|history| history.map(Payload::History).unwrap_or(Payload::Unchanged))
                    }
                };
                if results.send(Completion { ticket, result }).is_err() { break; }
            }
        }).map_err(|error| error.to_string())?;
        Ok(Self {
            sender,
            receiver,
            pending: None,
        })
    }
}

pub(crate) fn same_project(left: &str, right: &str) -> bool {
    fn canonical(path: &str) -> std::path::PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| std::path::PathBuf::from(path))
    }
    canonical(left) == canonical(right)
}
