//! Transcript saves, off the UI thread.
//!
//! A transcript can run to tens of megabytes once images are inlined, and while
//! a turn streams its revision changes on every delta. Encoding and writing it
//! on the UI thread made every checkpoint a visible stall, so the UI now only
//! decides *that* a thread needs saving. This worker decides what to write.
//!
//! One worker thread, and at most one waiting job per thread:
//!
//! - **A newer request replaces a waiting one** rather than queueing behind it.
//!   That bounds the queue by the number of loaded threads, and it means an
//!   older snapshot can never land after a newer one.
//! - **The transcript is read when the job runs, not when it is queued**, so a
//!   job writes the newest history there is. Only the fields the UI owns — the
//!   draft, the retry copy, the restored cost — travel with the job.
//! - **What was written is remembered as a length and a hash, not as bytes.** An
//!   unchanged transcript costs an encode and never a write, and nothing keeps a
//!   second copy of every saved conversation alive.

use std::collections::{HashMap, VecDeque};
use std::hash::Hasher;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use fx_store::{EncodedConversation, PersistedConversation, Store};
use uuid::Uuid;

use crate::state::GuardedConversation;

/// One thread's save, as the UI asked for it.
pub(crate) struct Job {
    /// Carried per job so a save always goes where the state's store points.
    pub store: Store,
    pub project_id: Uuid,
    pub agent_id: Uuid,
    pub conversation: Arc<GuardedConversation>,
    pub draft: String,
    /// A rejected prompt to offer again when the composer is otherwise empty.
    pub retry_draft: Option<String>,
    /// The thread's goal, which lives in the same file.
    pub goal: Option<fx_core::models::ConversationGoal>,
    pub cost: f64,
    /// The images of the thread's queued prompts, by the queued prompt's id.
    /// The queue's text is the conversation's; its images are the composer's.
    pub queued_attachments: Vec<(Uuid, Arc<Vec<fx_core::Attachment>>)>,
}

/// How a save ended, for the UI to report.
pub(crate) struct Outcome {
    pub agent_id: Uuid,
    /// The error is already worded for the user.
    pub result: Result<Saved, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Saved {
    Written,
    /// Identical to the last write, so nothing touched the disk.
    Unchanged,
    /// Nothing worth keeping: the files were removed rather than stubbed.
    Removed,
    /// Over the cache limit: the draft and session were written beside the
    /// history the cache already held.
    Partial,
}

/// Counts a test can assert on instead of timing anything.
#[derive(Default)]
pub(crate) struct Stats {
    pub encodes: AtomicUsize,
    pub writes: AtomicUsize,
    pub previous_reads: AtomicUsize,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Job>,
    /// The thread whose save is running now.
    writing: Option<Uuid>,
    /// The latest outcome per thread, so an idle session cannot pile them up.
    finished: Vec<Outcome>,
    /// Threads whose files were deleted. What the worker remembers about them
    /// goes too, before it takes another job.
    forgotten: Vec<Uuid>,
    closed: bool,
    #[cfg(test)]
    held: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
    stats: Stats,
    /// The cache limit, when a test lowers it; zero means the format's own.
    limit: AtomicU64,
}

impl Queue {
    fn record(&mut self, outcome: Outcome) {
        self.finished
            .retain(|earlier| earlier.agent_id != outcome.agent_id);
        self.finished.push(outcome);
    }
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn wait<'a>(&self, queue: MutexGuard<'a, Queue>) -> MutexGuard<'a, Queue> {
        self.changed
            .wait(queue)
            .unwrap_or_else(|error| error.into_inner())
    }

    fn limit(&self) -> u64 {
        match self.limit.load(Ordering::Relaxed) {
            0 => fx_store::format::MAX_CONVERSATION_BYTES,
            limit => limit,
        }
    }
}

/// The UI's handle on the worker.
#[derive(Default)]
pub(crate) struct ConversationWriter {
    shared: Arc<Shared>,
    started: bool,
    /// Used only if the worker thread could not be started: saving on the UI
    /// thread is slow, but losing a transcript is worse.
    inline: Option<Memory>,
}

impl ConversationWriter {
    /// Queue a save, replacing one for the same thread that has not started.
    pub fn submit(&mut self, job: Job) {
        self.start();
        if let Some(memory) = &mut self.inline {
            let agent_id = job.agent_id;
            let result = memory.save(job, &self.shared);
            self.shared.queue().record(Outcome { agent_id, result });
            return;
        }
        let mut queue = self.shared.queue();
        match queue
            .waiting
            .iter_mut()
            .find(|waiting| waiting.agent_id == job.agent_id)
        {
            Some(waiting) => *waiting = job,
            None => queue.waiting.push_back(job),
        }
        drop(queue);
        self.shared.changed.notify_all();
    }

    /// Outcomes since the last call.
    pub fn take_outcomes(&self) -> Vec<Outcome> {
        std::mem::take(&mut self.shared.queue().finished)
    }

    /// Block until every queued save has landed. For quitting and for explicit
    /// saves; a checkpoint never waits.
    pub fn flush(&self) {
        let mut queue = self.shared.queue();
        while !queue.waiting.is_empty() || queue.writing.is_some() {
            queue = self.shared.wait(queue);
        }
    }

    /// Drop a thread's waiting save and wait out one already running, so that
    /// deleting its files cannot be followed by a write that recreates them.
    pub fn forget(&mut self, agent_id: Uuid) {
        let mut queue = self.shared.queue();
        queue.waiting.retain(|job| job.agent_id != agent_id);
        queue
            .finished
            .retain(|outcome| outcome.agent_id != agent_id);
        if let Some(memory) = &mut self.inline {
            memory.forget(agent_id);
            return;
        }
        queue.forgotten.push(agent_id);
        while queue.writing == Some(agent_id) {
            queue = self.shared.wait(queue);
        }
        drop(queue);
        self.shared.changed.notify_all();
    }

    #[cfg(test)]
    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// Stop the worker from taking jobs, so a test can see what is queued.
    #[cfg(test)]
    pub fn hold(&self, held: bool) {
        self.shared.queue().held = held;
        self.shared.changed.notify_all();
    }

    #[cfg(test)]
    pub fn waiting(&self) -> usize {
        self.shared.queue().waiting.len()
    }

    /// Lower the cache limit, so a test need not encode tens of megabytes.
    #[cfg(test)]
    pub fn set_limit(&self, bytes: u64) {
        self.shared.limit.store(bytes, Ordering::Relaxed);
    }

    fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let shared = self.shared.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("conversation-writer".into())
            .spawn(move || run(&shared))
        {
            tracing::error!(%error, "could not start the transcript writer; saving inline");
            self.inline = Some(Memory::default());
        }
    }
}

impl Drop for ConversationWriter {
    /// The worker finishes what is queued and then exits. Nothing waits for it:
    /// quitting flushes explicitly, and a drop elsewhere must not block.
    fn drop(&mut self) {
        self.shared.queue().closed = true;
        self.shared.changed.notify_all();
    }
}

fn run(shared: &Shared) {
    let mut memory = Memory::default();
    loop {
        let job = {
            let mut queue = shared.queue();
            loop {
                for agent_id in std::mem::take(&mut queue.forgotten) {
                    memory.forget(agent_id);
                }
                #[cfg(test)]
                let held = queue.held && !queue.closed;
                #[cfg(not(test))]
                let held = false;
                if !held && let Some(job) = queue.waiting.pop_front() {
                    queue.writing = Some(job.agent_id);
                    break job;
                }
                if queue.closed && queue.waiting.is_empty() {
                    return;
                }
                queue = shared.wait(queue);
            }
        };
        let agent_id = job.agent_id;
        // A panic must still clear `writing`, or quitting would wait forever.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| memory.save(job, shared)))
                .unwrap_or_else(|_| {
                    Err("Could not save the conversation: the writer failed.".into())
                });
        let mut queue = shared.queue();
        queue.record(Outcome { agent_id, result });
        queue.writing = None;
        drop(queue);
        shared.changed.notify_all();
    }
}

/// What a write left on disk, as much as is needed to recognise it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fingerprint {
    Bytes { len: usize, hash: u64 },
    Removed,
}

impl Fingerprint {
    fn of(bytes: &[u8]) -> Self {
        let mut hasher = std::hash::DefaultHasher::new();
        hasher.write(bytes);
        Self::Bytes {
            len: bytes.len(),
            hash: hasher.finish(),
        }
    }
}

/// What the worker remembers between jobs. Lives on the worker's thread.
#[derive(Default)]
struct Memory {
    written: HashMap<Uuid, Fingerprint>,
    /// For a thread over the cache limit: the history its cache file held when
    /// it outgrew the limit. Read from disk once, then reused for as long as the
    /// thread stays over, instead of being re-read on every save.
    previous: HashMap<Uuid, Result<Option<PersistedConversation>, String>>,
}

impl Memory {
    fn forget(&mut self, agent_id: Uuid) {
        self.written.remove(&agent_id);
        self.previous.remove(&agent_id);
    }

    fn save(&mut self, job: Job, shared: &Shared) -> Result<Saved, String> {
        let stats = &shared.stats;
        let Job {
            store,
            project_id,
            agent_id,
            conversation,
            draft,
            retry_draft,
            goal,
            cost,
            queued_attachments,
        } = job;
        // The clone happens under the lock and nothing else does: encoding a
        // large transcript while holding it would stall the runner and the view.
        let mut persisted = {
            let conversation = conversation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            crate::state::persisted_from(&conversation)
        };
        persisted.total_cost_usd = cost;
        persisted.draft_text = draft;
        persisted.goal = goal;
        // Copied here, off the UI thread, rather than into every job.
        for prompt in &mut persisted.queued {
            if let Some((_, images)) = queued_attachments.iter().find(|(id, _)| *id == prompt.id) {
                prompt.attachments = images.as_ref().clone();
            }
        }
        // Whitespace is not a draft; the rejected prompt takes its place.
        if persisted.draft_text.trim().is_empty()
            && let Some(prompt) = retry_draft
        {
            persisted.draft_text = prompt;
        }

        if !persisted.is_worth_persisting() {
            self.previous.remove(&agent_id);
            if self.written.get(&agent_id) == Some(&Fingerprint::Removed) {
                return Ok(Saved::Unchanged);
            }
            store
                .delete_conversation(project_id, agent_id)
                .map_err(|error| format!("Could not save the conversation: {error}"))?;
            self.written.insert(agent_id, Fingerprint::Removed);
            return Ok(Saved::Removed);
        }

        stats.encodes.fetch_add(1, Ordering::Relaxed);
        let mut encoded = EncodedConversation::new(&persisted)
            .map_err(|error| format!("Could not save the conversation: {error}"))?;
        let limit = shared.limit();
        let partial = encoded.len() as u64 > limit;
        if partial {
            // A full native transcript can exceed the cache limit. Its draft is
            // local-only and must still survive quitting.
            let previous = self.previous.entry(agent_id).or_insert_with(|| {
                stats.previous_reads.fetch_add(1, Ordering::Relaxed);
                store
                    .load_conversation(project_id, agent_id)
                    .map_err(|error| error.to_string())
            });
            let previous = previous.clone().map_err(|error| {
                format!(
                    "Could not preserve this large conversation's draft without replacing an unreadable cache: {error}"
                )
            })?;
            encoded = crate::state::encode_partial_cache(persisted, previous, limit)?.1;
        } else {
            self.previous.remove(&agent_id);
        }

        let fingerprint = Fingerprint::of(&encoded);
        if self.written.get(&agent_id) != Some(&fingerprint) {
            store
                .save_encoded_conversation(project_id, agent_id, &encoded)
                .map_err(|error| format!("Could not save the conversation: {error}"))?;
            stats.writes.fetch_add(1, Ordering::Relaxed);
            self.written.insert(agent_id, fingerprint);
        } else if !partial {
            return Ok(Saved::Unchanged);
        }
        Ok(if partial {
            Saved::Partial
        } else {
            Saved::Written
        })
    }
}
