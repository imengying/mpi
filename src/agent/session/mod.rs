//! Session persistence: one linear JSONL file per session.
//!
//! Records are typed and separated the way codex separates them, which is the point:
//! conversation items and environment snapshots live in different records, so the
//! environment never enters the transcript (and never disturbs the prompt-cache prefix)
//! and is not treated as conversation by compaction.
//!
//! The file *is* the session. Normal writes append records; recovery may remove an
//! interrupted final record while holding the writer lock. The name is appended as
//! its own `session_info` record, and a compaction appends a `compacted` checkpoint
//! rather than replacing history. A session therefore only grows by "user messages +
//! the most recent window + checkpoints", not by the whole transcript.
//!
//! # Where things live
//!
//! This module owns [`Session`] itself: what a session is, how it is created, and every
//! method that appends a record. The work around it is split by what it touches, because
//! the pieces are read for different reasons and change for different causes:
//!
//! * `record` — one line of the file, typed. The data, and its validation.
//! * `file` — the descriptor: locking, appending, recovering a torn tail, deleting.
//! * `context` — what the model would be sent, projected over the records.
//! * `store` — the directory of sessions: listing, previews, id-prefix lookup.
//!
//! The split is not cosmetic: `context` does no IO and needs no lock, `store` must never
//! lock or repair a file somebody is writing, and mixing the two with the append path in one
//! file is how a listing accidentally repaired someone's live session.

use std::path::{Path, PathBuf};

use crate::config::Usage;

use self::dirs::sessions_dir;
use crate::llm::{Message, StopReason};

mod context;
pub mod dirs;
mod file;
mod record;
mod store;

pub use file::SessionError;

pub(crate) use file::Storage;

#[cfg(test)]
mod tests;

pub use record::{ContextSnapshot, Record, SessionHeader};
pub use store::{SessionSummary, find_by_prefix, list, message_preview, now};

/// An append handle whose writer lock is released even if a child inherited its descriptor.
/// A session, plus the bookkeeping needed to append safely.
pub struct Session {
    header: SessionHeader,
    path: PathBuf,
    /// The directory the session belongs to, kept apart from `header.cwd`: the header
    /// records where the session *started* and is never rewritten, while a resume in another
    /// directory relocates the session — and the store follows where the work is now.
    cwd: PathBuf,
    /// The store this session registers itself in once it has something to store.
    ///
    /// `Some` for a session the user started (the directory id is minted on the first write),
    /// `None` for one created at an explicit path, which is what the tests use — a test's
    /// temporary directory must not acquire an entry in the real table.
    register_under: Option<PathBuf>,
    storage: Storage,
    last_id: Option<String>,
    records: Vec<Record>,
    /// Cumulative usage over everything written so far, for the footer.
    pub totals: Usage,
    /// The most recent provider-reported usage and where it came from, so the
    /// threshold check can use real numbers instead of an estimate.
    pub last_usage: Option<Usage>,
    pub last_usage_index: Option<usize>,
    recovery_notes: Vec<String>,
}

impl Session {
    /// Start a new session in the store for `cwd`.
    ///
    /// Registration happens here rather than on lookup: a session is what gives a directory
    /// its entry in the table, so running pi somewhere and leaving is not recorded.
    pub fn create(cwd: &Path, model: &str) -> Result<Self, SessionError> {
        // The id is provisional until something is written: registering here would put every
        // directory pi is started in into the table, and the table is meant to say where
        // sessions *are* (see [`Session::persist`]).
        let mut session = Self::create_in(&sessions_dir(cwd), cwd, model)?;
        session.register_under = Some(dirs::sessions_root());
        Ok(session)
    }

    /// Start a new session under `dir`. The directory is a parameter so tests do not have
    /// to mutate process-wide environment variables.
    pub fn create_in(dir: &Path, cwd: &Path, model: &str) -> Result<Self, SessionError> {
        // The directory is not created here. A session that never says anything must leave
        // no trace at all — not a file, and not an empty directory in the store either,
        // which is what would otherwise happen to every directory pi is run in.
        let id = uuid::Uuid::now_v7().to_string();
        let path = dir.join(format!("{id}.jsonl"));
        let header = SessionHeader {
            id: id.clone(),
            timestamp: now(),
            cwd: cwd.to_string_lossy().to_string(),
            model: model.to_string(),
        };
        // Nothing is written here. A file appears with the first record that makes this a
        // conversation, so launching pi and leaving does not add a session to the list.
        let record = Record::SessionMeta {
            header: header.clone(),
        };
        Ok(Session {
            header,
            path,
            cwd: cwd.to_path_buf(),
            register_under: None,
            storage: Storage::Buffered,
            last_id: Some(id),
            records: vec![record],
            totals: Usage::default(),
            last_usage: None,
            last_usage_index: None,
            recovery_notes: Vec::new(),
        })
    }

    pub fn recovery_notes(&self) -> &[String] {
        &self.recovery_notes
    }

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// The session id, which is also the file stem: `~/.pi/sessions/<dir>/<id>.jsonl`.
    pub fn id(&self) -> &str {
        &self.header.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// Execution facts come from original records, including work before checkpoints.
    /// The directory in the newest turn context. A new session uses its header.
    pub fn current_cwd(&self) -> Option<PathBuf> {
        self.records.iter().rev().find_map(|record| match record {
            Record::TurnContext { cwd, .. } => Some(PathBuf::from(cwd)),
            _ => None,
        })
    }

    /// The model and thinking level of the newest recorded turn.
    ///
    /// `header.model` is what the session was *created* with and never changes; `/model`
    /// afterward would be forgotten on the next resume, and the session would quietly go back
    /// to a model the user had moved off. The turn contexts record what each turn actually
    /// ran with, so the newest one is the answer.
    ///
    /// `None` until the first turn or model selection is recorded.
    pub fn current_model(&self) -> Option<(String, String)> {
        self.records.iter().rev().find_map(|record| match record {
            Record::TurnContext { model, level, .. } if !model.is_empty() => {
                Some((model.clone(), level.clone()))
            }
            _ => None,
        })
    }

    /// What the user typed, oldest first, for the input history of a resumed screen.
    ///
    /// Only the user's own lines: the model's answers, the tool traffic and the environment
    /// block are all in the file, and none of them is a line the user can recall and send
    /// again. Reading from the records rather than from the context means a compacted
    /// session still offers the turns the summary folded away — they are what the user
    /// actually typed, which is what the arrows are for.
    ///
    /// A multi-paragraph message stays one history entry.
    pub fn user_history(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in &self.records {
            let Some(message) = record.message() else {
                continue;
            };
            if !matches!(message, Message::User { .. })
                || crate::agent::r#loop::is_environment_block(message)
            {
                continue;
            }
            let text = message.text();
            if !text.trim().is_empty() {
                out.push(text);
            }
        }
        out
    }

    /// Record a new working directory while preserving the latest model and level.
    pub fn relocate(&mut self, cwd: &Path) -> Result<(), SessionError> {
        if self.cwd == cwd {
            return Ok(());
        }
        let (model, level) = self
            .current_model()
            .unwrap_or_else(|| (self.header.model.clone(), String::new()));
        let id = self.next_id();
        let record = Record::TurnContext {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model,
            level,
            timestamp: now(),
        };
        self.append(record, id)?;
        self.cwd = cwd.to_path_buf();
        // The file moves with it. A session is about the directory it is being continued in,
        // and the store says which directory that is — leaving the file behind would make it
        // invisible to `/resume` where it now belongs, while still claiming, from its old
        // home, to be a session about somewhere else. The new path is registered first; if
        // that fails the session keeps its old one and stays findable there.
        self.follow_cwd()?;
        Ok(())
    }

    /// Move the file into the store of [`Session::cwd`], if it is not already there.
    ///
    /// The open handle keeps working across the rename — it names the file, not the path —
    /// so later appends land in the moved file.
    fn follow_cwd(&mut self) -> Result<(), SessionError> {
        if self.register_under.is_none() {
            return Ok(());
        }
        let root = dirs::sessions_root();
        let id = dirs::register_dir_in(&root, &self.cwd);
        let name = match self.path.file_name() {
            Some(name) => name.to_owned(),
            None => return Ok(()),
        };
        let target = root.join(id).join(name);
        if target == self.path {
            return Ok(());
        }
        std::fs::create_dir_all(target.parent().unwrap_or(&root))?;
        std::fs::rename(&self.path, &target)?;
        self.path = target;
        Ok(())
    }

    /// The last `session_info` record wins; the header is never rewritten. An explicit
    /// `null` name clears it, which is why the record type keeps `Option`.
    pub fn name(&self) -> Option<String> {
        self.records
            .iter()
            .rev()
            .find_map(|record| match record {
                Record::SessionInfo { name, .. } => Some(name.clone()),
                _ => None,
            })
            .flatten()
    }

    pub fn set_name(&mut self, name: Option<&str>) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::SessionInfo {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            name: name.map(|n| n.to_string()),
            timestamp: now(),
        };
        self.append(record, id)
    }

    /// Append a conversation item.
    pub fn push_message(
        &mut self,
        message: Message,
        usage: Option<Usage>,
        stop_reason: Option<StopReason>,
    ) -> Result<(), SessionError> {
        let usage = usage.filter(Usage::is_meaningful);
        let id = self.next_id();
        let record = Record::ResponseItem {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            message,
            usage,
            stop_reason,
            timestamp: now(),
        };
        self.append(record, id.clone())?;
        if let Some(usage) = usage {
            self.last_usage = Some(usage);
            self.last_usage_index = Some(self.records.len() - 1);
        }
        Ok(())
    }

    /// Record the per-turn environment snapshot. It is *not* part of the conversation.
    pub fn push_turn_context(
        &mut self,
        cwd: &Path,
        model: &str,
        level: &str,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::TurnContext {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model: model.to_string(),
            level: level.to_string(),
            timestamp: now(),
        };
        self.append(record, id)
    }

    /// Persist the exact request projection before the provider call starts.
    pub fn push_request_context(
        &mut self,
        request_id: &str,
        request: &crate::llm::Request<'_>,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let system_prompt_hash = request.messages.iter().find_map(|message| match message {
            Message::System { content } => Some(crate::util::stable_digest(content)),
            _ => None,
        });
        let tools_hash = crate::util::stable_digest(
            &serde_json::to_string(request.tools).expect("tool schema is serializable"),
        );
        let record = Record::RequestContext {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            request_id: request_id.to_string(),
            provider: request.provider.name.clone(),
            model: request.model.id.clone(),
            level: request.level.to_string(),
            context_window: request.model.context_window,
            token_estimate: crate::llm::estimate_request_context(
                request.messages,
                "",
                request.tools,
            ),
            system_prompt_hash,
            tools_hash,
            cache_hints: request.cache_hints,
            timestamp: now(),
        };
        self.append(record, id)
    }

    pub fn push_compaction_start(
        &mut self,
        compaction_id: &str,
        reason: &str,
        provider: &str,
        model: &str,
        level: &str,
        token_before: u64,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::CompactionStart {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            compaction_id: compaction_id.to_string(),
            reason: reason.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            level: level.to_string(),
            token_before,
            timestamp: now(),
        };
        self.append(record, id)
    }

    pub fn push_compaction_end(
        &mut self,
        compaction_id: &str,
        error: Option<&str>,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::CompactionEnd {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            compaction_id: compaction_id.to_string(),
            error: error.map(str::to_string),
            timestamp: now(),
        };
        self.append(record, id)
    }

    /// Append a compaction checkpoint. The original messages stay on disk but stop being
    /// part of the context.
    pub fn push_compaction(
        &mut self,
        outcome: crate::agent::compact::CompactionOutcome,
        reason: &str,
        provider: &str,
        model: &str,
        level: &str,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let usage = outcome.usage.is_meaningful().then_some(outcome.usage);
        let record = Record::Compacted {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            compaction_id: outcome.compaction_id,
            reason: reason.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            level: level.to_string(),
            summary: outcome.summary,
            replacement_history: outcome.replacement,
            replacement_ids: outcome.replacement_ids,
            read_files: outcome.read_files,
            modified_files: outcome.modified_files,
            shadowed_ids: outcome.shadowed_ids,
            token_before: outcome.token_before,
            token_after: outcome.token_after,
            summary_prompt_version: crate::agent::compact::SUMMARY_PROMPT_VERSION.to_string(),
            usage,
            timestamp: now(),
        };
        self.append(record, id)?;
        // The new context is smaller than anything measured so far, so the old usage
        // must not be reused for the threshold check.
        self.last_usage = None;
        self.last_usage_index = None;
        Ok(())
    }

    pub fn push_tool_start(
        &mut self,
        tool_call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        replay_safe: bool,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::ToolExecutionStart {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            arguments,
            replay_safe,
            timestamp: now(),
        };
        self.append(record, id)
    }

    pub fn push_tool_end(
        &mut self,
        tool_call_id: &str,
        name: &str,
        status: crate::llm::ToolStatus,
        duration_ms: Option<u64>,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::ToolExecutionEnd {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            status,
            duration_ms,
            timestamp: now(),
        };
        self.append(record, id)
    }

    pub fn push_pruning(
        &mut self,
        outcome: crate::agent::compact::PruneOutcome,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let replacement_ids = self.context_snapshot().entry_ids;
        let record = Record::Pruned {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            replacement_history: outcome.replacement,
            replacement_ids,
            tool_results: outcome.tool_results,
            saved_tokens: outcome.saved_tokens,
            timestamp: now(),
        };
        self.append(record, id)?;
        self.last_usage = None;
        self.last_usage_index = None;
        Ok(())
    }

    /// Whether this session has a file on disk right now, and therefore something
    /// `/resume` could bring back.
    pub fn is_saved(&self) -> bool {
        matches!(self.storage, Storage::Open(_))
    }

    fn next_id(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }

    fn recompute_usage(&mut self) {
        let mut totals = Usage::default();
        let mut last_usage = None;
        let mut last_index = None;
        for (index, record) in self.records.iter().enumerate() {
            if let Some(usage) = record.usage() {
                totals.add(&usage);
                if usage.is_meaningful() {
                    last_usage = Some(usage);
                    last_index = Some(index);
                }
            }
        }
        // Usage recorded before the last checkpoint describes the *old*, larger context,
        // so it is not a valid reading for the threshold check.
        if let Some(checkpoint) = self.last_checkpoint_index()
            && last_index.map(|index| index <= checkpoint).unwrap_or(true)
        {
            last_usage = None;
            last_index = None;
        }
        self.totals = totals;
        self.last_usage = last_usage;
        self.last_usage_index = last_index;
    }
}
