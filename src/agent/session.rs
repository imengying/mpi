//! Session persistence: one linear JSONL file per session.
//!
//! Records are typed and separated the way codex separates them, which is the point:
//! conversation items and environment snapshots live in different records, so the
//! environment never enters the transcript (and never disturbs the prompt-cache prefix)
//! and is not treated as conversation by compaction.
//!
//! The file *is* the session. Nothing rewrites earlier bytes — the name is appended as
//! its own `session_info` record, and a compaction appends a `compacted` checkpoint
//! rather than replacing history. A session therefore only grows by "user messages +
//! the most recent window + checkpoints", not by the whole transcript.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Usage, sessions_dir};
use crate::llm::{Block, Message, StopReason};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub model: String,
}

/// One line of the file. `type` is the discriminator, matching codex's shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    /// First line: id, time, cwd, model.
    SessionMeta {
        #[serde(flatten)]
        header: SessionHeader,
    },
    /// A conversation item (user / assistant / tool result).
    ResponseItem {
        /// Links back to the previous record, forming the chain used by `/resume`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        id: String,
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    /// Per-turn environment snapshot. Never sent to the model as conversation.
    TurnContext {
        parent_id: Option<String>,
        id: String,
        cwd: String,
        model: String,
        level: String,
        timestamp: String,
    },
    /// Out-of-band event, including the per-request token count.
    EventMsg {
        parent_id: Option<String>,
        id: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        timestamp: String,
    },
    /// The session name. Appended, never a rewrite of the header.
    SessionInfo {
        parent_id: Option<String>,
        id: String,
        #[serde(default)]
        name: Option<String>,
        timestamp: String,
    },
    /// A compaction checkpoint: the replacement history plus the summary it came from.
    Compacted {
        parent_id: Option<String>,
        id: String,
        reason: String,
        summary: String,
        /// Full message list that supersedes everything before this record.
        replacement_history: Vec<Message>,
        read_files: Vec<String>,
        modified_files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        timestamp: String,
    },
}

impl Record {
    pub fn id(&self) -> &str {
        match self {
            Record::SessionMeta { header, .. } => &header.id,
            Record::ResponseItem { id, .. }
            | Record::TurnContext { id, .. }
            | Record::EventMsg { id, .. }
            | Record::SessionInfo { id, .. }
            | Record::Compacted { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Record::SessionMeta { .. } => None,
            Record::ResponseItem { parent_id, .. }
            | Record::TurnContext { parent_id, .. }
            | Record::EventMsg { parent_id, .. }
            | Record::SessionInfo { parent_id, .. }
            | Record::Compacted { parent_id, .. } => parent_id.as_deref(),
        }
    }

    /// Only conversation items contribute to the model's context.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Record::ResponseItem { message, .. } => Some(message),
            _ => None,
        }
    }

    /// Whether this record is the one that makes a session worth keeping.
    ///
    /// The environment block is bookkeeping every session starts with, and a name, a model
    /// switch or a turn context on its own is not a conversation. Only a real message —
    /// what the user said, or what came back to them — earns a file.
    fn starts_a_conversation(&self) -> bool {
        match self.message() {
            Some(message) => !crate::agent::r#loop::is_environment_block(message),
            None => false,
        }
    }

    pub fn usage(&self) -> Option<Usage> {
        match self {
            Record::ResponseItem { usage, .. }
            | Record::EventMsg { usage, .. }
            | Record::Compacted { usage, .. } => *usage,
            _ => None,
        }
    }
}

/// Where a session's records live.
///
/// Three states, and the difference between the first two is the whole point: a session
/// that has not happened yet must leave no file behind, and a session the user deleted must
/// never get one again. `Option<File>` could not tell those apart.
enum Storage {
    /// No file yet. Records are held in memory until the session becomes a conversation.
    Buffered,
    /// The file exists and is open for appending.
    Open(std::fs::File),
    /// The file is gone. Nothing may create it again.
    Deleted,
}

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
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("无法读写会话文件：{0}")]
    Io(#[from] std::io::Error),
    #[error("会话文件格式无法识别：{0}")]
    Parse(String),
    #[error("会话已被删除")]
    Deleted,
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
        session.register_under = Some(crate::config::sessions_root());
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
        let record = Record::SessionMeta { header: header.clone() };
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
        })
    }

    /// Open an existing session file and replay it.
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut records: Vec<Record> = Vec::new();
        for (number, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let record: Record = serde_json::from_str(&line).map_err(|err| {
                SessionError::Parse(format!("{} 第 {} 行：{err}", path.display(), number + 1))
            })?;
            records.push(record);
        }
        let header = records
            .iter()
            .find_map(|record| match record {
                Record::SessionMeta { header, .. } => Some(header.clone()),
                _ => None,
            })
            .ok_or_else(|| SessionError::Parse(format!("{} 缺少会话头", path.display())))?;
        let last_id = records.last().map(|record| record.id().to_string());
        let cwd = PathBuf::from(&header.cwd);
        let mut session = Session {
            header,
            path: path.to_path_buf(),
            cwd,
            // A resumed session is a live one: if it is continued in another directory its
            // file follows, the same as a session that was never interrupted.
            register_under: Some(crate::config::sessions_root()),
            storage: Storage::Open(std::fs::OpenOptions::new().append(true).open(path)?),
            last_id,
            records,
            totals: Usage::default(),
            last_usage: None,
            last_usage_index: None,
        };
        session.recompute_usage();
        Ok(session)
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

    /// The conversation as the model should see it: everything after the last
    /// checkpoint, using the checkpoint's replacement history in its place, minus any
    /// assistant message an overflow recovery dropped.
    pub fn context_messages(&self) -> Vec<Message> {
        let start = self.last_checkpoint_index();
        let mut messages: Vec<Message> = match start {
            Some(index) => {
                let Record::Compacted { replacement_history, .. } = &self.records[index] else {
                    unreachable!()
                };
                let mut messages = replacement_history.clone();
                for record in &self.records[index + 1..] {
                    if let Some(message) = record.message() {
                        messages.push(message.clone());
                    }
                }
                messages
            }
            None => self.records.iter().filter_map(Record::message).cloned().collect(),
        };
        // A `dropped_assistant` marker removes the matching message by position, newest
        // first, so the chain can be reconstructed without rewriting the file.
        let dropped = self
            .records
            .iter()
            .filter(|record| {
                matches!(record, Record::EventMsg { kind, .. } if kind == "dropped_assistant")
            })
            .count();
        for _ in 0..dropped {
            if let Some(index) = messages
                .iter()
                .rposition(|message| matches!(message, Message::Assistant { .. }))
            {
                messages.remove(index);
            }
        }
        messages
    }

    pub fn last_checkpoint_index(&self) -> Option<usize> {
        self.records
            .iter()
            .rposition(|record| matches!(record, Record::Compacted { .. }))
    }

    /// The working directory named by the newest environment block in the context.
    ///
    /// `header.cwd` is what the session *started* with and never changes; after a resume in
    /// a different directory it is the environment block, not the header, that says where
    /// the work is happening now.
    pub fn current_cwd(&self) -> Option<PathBuf> {
        self.context_messages().iter().rev().find_map(|message| {
            crate::agent::r#loop::is_environment_block(message)
                .then(|| {
                    message
                        .text()
                        .lines()
                        .find_map(|line| line.strip_prefix("工作目录: ").map(PathBuf::from))
                })
                .flatten()
        })
    }

    /// The model and thinking level of the newest recorded turn.
    ///
    /// `header.model` is what the session was *created* with and never changes; `/model`
    /// afterward would be forgotten on the next resume, and the session would quietly go back
    /// to a model the user had moved off. The turn contexts record what each turn actually
    /// ran with, so the newest one is the answer.
    ///
    /// `None` for a session whose turns predate this record, or that never got past its first
    /// message — the caller falls back to the header, which is then the honest answer.
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
    /// Multi-line input is stored as one message but the editor is single-line, so each line
    /// becomes its own history entry: recalling a message that was typed over three lines
    /// would otherwise drop a newline into a buffer that cannot hold one.
    pub fn user_history(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for record in &self.records {
            let Some(message) = record.message() else { continue };
            if !matches!(message, Message::User { .. }) || crate::agent::r#loop::is_environment_block(message) {
                continue;
            }
            for line in message.text().lines() {
                if !line.trim().is_empty() {
                    out.push(line.to_string());
                }
            }
        }
        out
    }

    /// Point the session header at `cwd`. The header line itself is never rewritten: the
    /// update is appended as a `session_info` record, like the session name.
    pub fn relocate(&mut self, cwd: &Path) -> Result<(), SessionError> {        if self.header.cwd == cwd.to_string_lossy() {
            return Ok(());
        }
        let id = self.next_id();
        let record = Record::SessionInfo {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            name: self.name(),
            timestamp: now(),
        };
        let _ = record;
        // `SessionInfo` carries only the name, so the directory update rides on the
        // turn-context record, which is exactly what that record type exists for.
        let record = Record::TurnContext {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            model: self.header.model.clone(),
            level: String::new(),
            timestamp: now(),
        };
        self.append(record, id)?;
        self.header.cwd = cwd.to_string_lossy().to_string();
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
        let root = crate::config::sessions_root();
        let id = crate::config::register_dir_in(&root, &self.cwd);
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
        let id = self.next_id();
        let record = Record::ResponseItem {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            message,
            usage,
            stop_reason,
            timestamp: Some(now()),
        };
        self.append(record, id.clone())?;
        if let Some(usage) = usage {
            self.last_usage = Some(usage);
            self.last_usage_index = Some(self.records.len() - 1);
        }
        Ok(())
    }

    /// Record the per-turn environment snapshot. It is *not* part of the conversation.
    pub fn push_turn_context(&mut self, cwd: &Path, model: &str, level: &str) -> Result<(), SessionError> {
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

    /// Record the token count a request actually used.
    pub fn push_token_count(&mut self, usage: Usage) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::EventMsg {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            kind: "token_count".into(),
            usage: Some(usage),
            timestamp: now(),
        };
        self.append(record, id)
    }

    /// Append a compaction checkpoint. The original messages stay on disk but stop being
    /// part of the context.
    #[allow(clippy::too_many_arguments)]
    pub fn push_compaction(
        &mut self,
        reason: &str,
        summary: &str,
        replacement_history: Vec<Message>,
        read_files: Vec<String>,
        modified_files: Vec<String>,
        usage: Option<Usage>,
    ) -> Result<(), SessionError> {
        let id = self.next_id();
        let record = Record::Compacted {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            reason: reason.to_string(),
            summary: summary.to_string(),
            replacement_history,
            read_files,
            modified_files,
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

    /// Remove the most recent assistant message from the live context.
    ///
    /// Used by the overflow path: a response that never completed must not be folded into
    /// the summary. The record stays on disk — nothing rewrites history — but it is
    /// excluded from the messages the model sees.
    pub fn drop_last_assistant(&mut self) -> Result<(), SessionError> {
        let Some(index) = self
            .records
            .iter()
            .rposition(|record| matches!(record.message(), Some(Message::Assistant { .. })))
        else {
            return Ok(());
        };
        self.records.remove(index);
        // Dropping the newest record invalidates the chain head and any usage reading that
        // came from it.
        self.last_id = self.records.last().map(|record| record.id().to_string());
        self.recompute_usage();
        self.last_usage = None;
        self.last_usage_index = None;
        // The exclusion has to survive a reopen, so it is recorded explicitly.
        let id = self.next_id();
        let record = Record::EventMsg {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            kind: "dropped_assistant".into(),
            usage: None,
            timestamp: now(),
        };
        self.append(record, id)
    }

    /// Delete the session file.
    ///
    /// The handle is closed first: an open handle keeps the file alive and, on Windows,
    /// blocks the unlink outright. After this the session is inert — [`Session::append`]
    /// refuses, so a late write cannot recreate the file the user just deleted.
    /// Returns whether there was a file to remove: a session that never became a
    /// conversation has nothing on disk, and claiming otherwise would be a lie.
    pub fn delete(&mut self) -> Result<bool, SessionError> {
        // Closing is what the drop does; taking it out of the field is what makes the
        // "already deleted" state representable. A buffered session goes straight there:
        // it has no file to unlink, but it must still be sealed so a late write cannot
        // create one behind the user's back.
        let removed = match self.storage {
            Storage::Buffered => false,
            Storage::Open(_) => true,
            Storage::Deleted => false,
        };
        if let Storage::Open(file) = std::mem::replace(&mut self.storage, Storage::Deleted) {
            drop(file);
            std::fs::remove_file(&self.path)?;
        }
        self.records.clear();
        self.last_id = None;
        Ok(removed)
    }

    fn append(&mut self, record: Record, id: String) -> Result<(), SessionError> {
        // The file is created by the record that turns an empty launch into a conversation;
        // until then everything is buffered, including the header and the environment block,
        // and is written out in one go. Keeping those out of the file is not only about
        // tidiness: a file whose only content is a header and an environment block is a
        // session in the resume list that has nothing to resume.
        if matches!(self.storage, Storage::Buffered) && record.starts_a_conversation() {
            self.persist()?;
        }
        match &mut self.storage {
            Storage::Buffered => {}
            Storage::Open(file) => {
                let line = serde_json::to_string(&record)
                    .map_err(|err| SessionError::Parse(err.to_string()))?;
                writeln!(file, "{line}")?;
                file.flush()?;
            }
            Storage::Deleted => return Err(SessionError::Deleted),
        }
        if let Some(usage) = record.usage() {
            self.totals.add(&usage);
        }
        self.last_id = Some(id);
        self.records.push(record);
        Ok(())
    }

    /// Create the file and write everything buffered so far, in order.
    fn persist(&mut self) -> Result<(), SessionError> {
        // The directory is registered now, because this is the moment the store gains
        // something to name. `create` only had a provisional id: registering there would
        // record every directory pi is ever started in, and the table is meant to answer
        // "where are the sessions", not "where has the user been".
        //
        // The registered id can differ from the provisional one, so the path is rebuilt from
        // it rather than reused. Both name the same directory; only the name changes, and it
        // changes before the file exists, so nothing has to be moved.
        if let Some(root) = self.register_under.clone() {
            let registered = crate::config::register_dir_in(&root, &self.cwd);
            let name = self
                .path
                .file_name()
                .map(|name| name.to_owned())
                .unwrap_or_default();
            self.path = root.join(registered).join(name);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        for record in &self.records {
            let line = serde_json::to_string(record)
                .map_err(|err| SessionError::Parse(err.to_string()))?;
            writeln!(file, "{line}")?;
        }
        file.flush()?;
        self.storage = Storage::Open(file);
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
                last_usage = Some(usage);
                last_index = Some(index);
            }
        }
        // Usage recorded before the last checkpoint describes the *old*, larger context,
        // so it is not a valid reading for the threshold check.
        if let Some(checkpoint) = self.last_checkpoint_index()
            && last_index.map(|index| index < checkpoint).unwrap_or(true)
        {
            last_usage = None;
            last_index = None;
        }
        self.totals = totals;
        self.last_usage = last_usage;
        self.last_usage_index = last_index;
    }
}

pub fn now() -> String {
    // RFC 3339 to the second, in UTC. `SystemTime` is enough here: the timestamp is
    // only ever shown to a human and compared for ordering.
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-from-civil, inverted. No calendar crate needed for one formatting function.
fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let rem = seconds % 86_400;
    let (hour, minute, second) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, minute, second)
}

/// A session as it appears in the `/resume` list.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub modified: std::time::SystemTime,
    pub messages: usize,
    /// First user message, used when the session has no name.
    pub snippet: String,
}

impl SessionSummary {
    /// The label shown in the list, truncated to one line.
    pub fn label(&self, width: usize) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => crate::util::truncate(name, width, "…"),
            _ => {
                let single = crate::util::one_line(&self.snippet);
                if single.is_empty() {
                    "(空白会话)".to_string()
                } else {
                    crate::util::truncate(&single, width, "…")
                }
            }
        }
    }

    pub fn modified_label(&self) -> String {
        let Ok(modified) = self.modified.duration_since(std::time::UNIX_EPOCH) else {
            return "未知时间".into();
        };
        let (year, month, day, hour, minute, _) = civil_from_unix(modified.as_secs());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age = now.saturating_sub(modified.as_secs());
        if age < 60 {
            "刚刚".into()
        } else if age < 3600 {
            format!("{} 分钟前", age / 60)
        } else if age < 86_400 {
            format!("{} 小时前", age / 3600)
        } else if age < 86_400 * 7 {
            format!("{} 天前", age / 86_400)
        } else {
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
        }
    }
}

/// List sessions in the default directory, newest first.
/// Find one session by id, or by any unique prefix of it.
///
/// Matching a prefix is what makes the id printed on exit usable: the full uuid is awkward
/// to retype, and the first few characters are already unique in practice. An ambiguous
/// prefix is an error rather than a guess — resuming the wrong conversation silently is
/// worse than asking for more characters.
pub fn find_by_prefix(prefix: &str, cwd: &Path) -> Result<PathBuf, String> {
    find_by_prefix_in(&sessions_dir(cwd), prefix)
}

/// [`find_by_prefix`] against a given directory, so tests do not touch the real one.
pub fn find_by_prefix_in(dir: &Path, prefix: &str) -> Result<PathBuf, String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Err("会话 id 不能为空".to_string());
    }
    let matches: Vec<SessionSummary> = list_in(dir)
        .into_iter()
        .filter(|summary| summary.id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(format!("找不到会话「{prefix}」")),
        1 => Ok(matches[0].path.clone()),
        n => {
            let shown: Vec<&str> = matches.iter().take(4).map(|m| m.id.as_str()).collect();
            Err(format!(
                "「{prefix}」匹配到 {n} 个会话，请多给几位：{}",
                shown.join("、")
            ))
        }
    }
}

/// Sessions recorded in `cwd`, newest first.
///
/// Only this directory's: a conversation belongs to the project it happened in, and
/// `/resume` listing every other project's history would bury the relevant ones.
pub fn list(cwd: &Path) -> Vec<SessionSummary> {
    list_in(&sessions_dir(cwd))
}

/// List sessions under `dir`. A session that cannot be parsed is skipped rather than
/// taking the whole list down with it.
pub fn list_in(dir: &Path) -> Vec<SessionSummary> {
    let dir = dir.to_path_buf();
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut summaries: Vec<SessionSummary> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let Ok(session) = Session::open(&path) else { continue };
        // The environment block is the first *user* message in the conversation, but it is
        // bookkeeping rather than something the user said, so it counts for neither the
        // message total nor the snippet.
        let is_environment = |record: &Record| -> bool {
            match record.message() {
                Some(message) => message.text().trim_start().starts_with("<environment>"),
                None => false,
            }
        };
        let messages = session
            .records()
            .iter()
            .filter(|record| record.message().is_some() && !is_environment(record))
            .count();
        let snippet = session
            .records()
            .iter()
            .find_map(|record| match record.message() {
                Some(message) if !is_environment(record) => Some(message.text()),
                _ => None,
            })
            .unwrap_or_default();
        summaries.push(SessionSummary {
            path,
            id: session.header().id.clone(),
            name: session.name(),
            modified,
            messages,
            snippet,
        });
    }
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    summaries
}

/// A one-line summary of a message, for the transcript and the resume list.
pub fn message_preview(message: &Message, width: usize) -> String {
    match message {
        Message::User { .. } => crate::util::truncate(&crate::util::one_line(&message.text()), width, "…"),
        Message::Assistant { content, .. } => {
            let text = content
                .iter()
                .map(|block| match block {
                    Block::Text { text } => text.clone(),
                    Block::Thinking { .. } => "[思考]".to_string(),
                    Block::ToolCall { name, .. } => format!("[{name}]"),
                    Block::Hosted { .. } => "[搜索]".to_string(),
                    Block::Citation { .. } => "[引用]".to_string(),
                    Block::Image { .. } => "[图片]".to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            crate::util::truncate(&crate::util::one_line(&text), width, "…")
        }
        Message::Tool { name, .. } => format!("[{name} 结果]"),
        Message::System { .. } => "[系统]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session(name: &str) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pi-session-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session::create_in(&dir, &dir, "work/m").unwrap();
        (session, dir)
    }

    /// A session that has already said something, and therefore has a file. Tests about
    /// listing, finding or deleting a session need one that exists on disk.
    fn temp_session_with_a_message(name: &str) -> (Session, PathBuf) {
        let (mut session, dir) = temp_session(name);
        session
            .push_message(Message::user_text("你好"), None, None)
            .unwrap();
        (session, dir)
    }

    #[test]
    fn user_history_holds_what_the_user_typed_and_nothing_else() {
        // Up on a resumed screen reaches back into the conversation. What it must *not*
        // reach is the model's own words: recalling an answer and sending it back would look
        // like the user saying something they never said.
        let (mut session, dir) = temp_session("history");
        session
            .push_message(Message::user_text("第一行\n第二行"), None, None)
            .unwrap();
        session
            .push_message(Message::assistant_text("回答"), None, None)
            .unwrap();
        session
            .push_message(
                Message::Tool {
                    tool_call_id: "c1".into(),
                    name: "bash".into(),
                    content: "输出".into(),
                },
                None,
                None,
            )
            .unwrap();
        session
            .push_message(Message::System { content: "系统".into() }, None, None)
            .unwrap();
        // The environment block is a user message by type but bookkeeping by intent.
        session
            .push_message(Message::user_text("<environment>\n工作目录: /tmp"), None, None)
            .unwrap();

        // Each line of a multi-line message is its own entry: the editor is single-line, so
        // recalling a message that was typed over two lines would drop a newline into a
        // buffer that cannot hold one.
        assert_eq!(session.user_history(), vec!["第一行".to_string(), "第二行".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_history_survives_a_compaction() {
        // Compaction replaces the context with a summary, but the arrows are about what the
        // user typed, and those turns did happen. Reading the records instead of the context
        // is what keeps them reachable.
        let (mut session, dir) = temp_session("history-compacted");
        session.push_message(Message::user_text("被压缩掉的话"), None, None).unwrap();
        session
            .push_compaction("test", "摘要", vec![Message::user_text("摘要占位")], vec![], vec![], None)
            .unwrap();
        assert_eq!(session.context_messages().len(), 1, "the summary replaced the turn");
        assert_eq!(session.user_history(), vec!["被压缩掉的话".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_can_be_found_by_an_id_prefix() {
        let (session, dir) = temp_session_with_a_message("find");
        let id = session.id().to_string();

        // The full id works, and so does any unique prefix of it — that is what makes the
        // command printed on exit usable without retyping 36 characters.
        assert_eq!(find_by_prefix_in(&dir, &id).unwrap(), session.path());
        assert_eq!(find_by_prefix_in(&dir, &id[..8]).unwrap(), session.path());
        // Whitespace from a sloppy copy-paste is tolerated.
        assert_eq!(find_by_prefix_in(&dir, &format!("  {}  ", &id[..8])).unwrap(), session.path());

        // An unknown id is an error: silently starting a blank session would hide the typo
        // until the user noticed the missing history.
        assert!(find_by_prefix_in(&dir, "ffffffff").is_err());
        assert!(find_by_prefix_in(&dir, "").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_rather_than_guessed() {
        let dir = std::env::temp_dir().join(format!("pi-ambig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Two sessions, so a short prefix can match both. uuids are time-ordered (v7), so
        // sessions created in the same moment share a long leading run — the common prefix
        // is what a user is most likely to type, which is exactly when guessing would be
        // worst.
        // Both sessions must have a file to appear in the list, so both say something.
        for _ in 0..2 {
            let mut session = Session::create_in(&dir, &dir, "work/m").unwrap();
            session
                .push_message(Message::user_text("你好"), None, None)
                .unwrap();
        }

        let ids: Vec<String> = list_in(&dir).into_iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), 2);
        // The longest prefix that still matches both: one character shorter than the point
        // where the two ids diverge.
        let shared = ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .iter()
            .fold(usize::MAX, |acc, id| {
                acc.min(
                    ids[0]
                        .chars()
                        .zip(id.chars())
                        .take_while(|(x, y)| x == y)
                        .count(),
                )
            });
        let ambiguous = &ids[0][..shared];
        assert!(
            shared > 0 && ids.iter().all(|id| id.starts_with(ambiguous)),
            "the constructor did not produce a shared prefix: {ids:?}"
        );
        let err = find_by_prefix_in(&dir, ambiguous).unwrap_err();
        assert!(err.contains("匹配到"), "{err}");
        assert!(err.contains("请多给几位"), "{err}");

        // A prefix one character longer than the shared run picks exactly one session.
        let unique = &ids[0][..shared + 1];
        assert_eq!(find_by_prefix_in(&dir, unique).unwrap(), dir_for(&dir, unique));
        // And the full id stays unambiguous.
        assert!(find_by_prefix_in(&dir, &ids[0]).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The path a session with this id prefix lives at.
    fn dir_for(dir: &Path, id_prefix: &str) -> PathBuf {
        list_in(dir)
            .into_iter()
            .find(|s| s.id.starts_with(id_prefix))
            .expect("the id names a session")
            .path
    }

    #[test]
    fn deleting_an_unsaved_session_reports_that_there_was_nothing_to_delete() {
        let (mut session, dir) = temp_session("delete-unsaved");
        assert!(!session.is_saved());
        // No file, so nothing was removed — and the caller must be told, or `/delete` would
        // offer an `rm` path that does not exist.
        assert!(!session.delete().unwrap());
        assert!(!session.path().exists());

        // A late write must still not create the file: the user asked for this session to
        // end, and re-creating it minutes later would be exactly what `/delete` prevents.
        let after = session.push_message(Message::user_text("迟到的消息"), None, None);
        assert!(matches!(after, Err(SessionError::Deleted)), "{after:?}");
        assert!(!session.path().exists());
        assert!(list_in(&dir).is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_with_nothing_in_it_still_gets_a_label() {
        // New sessions no longer produce such a file, but older ones exist on disk, and a
        // `/resume` row with an empty description would look like a bug.
        let summary = SessionSummary {
            path: PathBuf::from("/tmp/x.jsonl"),
            id: "01a0b8d0".into(),
            name: None,
            modified: std::time::SystemTime::UNIX_EPOCH,
            messages: 0,
            snippet: String::new(),
        };
        assert_eq!(summary.label(40), "(空白会话)");
        // A name wins over the missing snippet.
        let named = SessionSummary { name: Some("我的会话".into()), ..summary };
        assert_eq!(named.label(40), "我的会话");
    }

    #[test]
    fn a_name_alone_does_not_create_a_session_file() {
        // Naming an empty session is not a conversation. It must not leave a file that
        // `/resume` would list with nothing to show.
        let (mut session, dir) = temp_session("name-only");
        session.set_name(Some("只有名字")).unwrap();
        assert!(!session.is_saved(), "a name is not a conversation");
        assert!(list_in(&dir).is_empty());

        // The first message persists the name too, which is what the user set it for.
        session
            .push_message(Message::user_text("你好"), None, None)
            .unwrap();
        assert!(session.is_saved());
        let reopened = Session::open(session.path()).unwrap();
        assert_eq!(reopened.name().as_deref(), Some("只有名字"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleting_removes_the_file_and_refuses_later_writes() {
        let (mut session, dir) = temp_session("delete");
        session.push_message(Message::user_text("hello"), None, None).unwrap();
        let path = session.path().to_path_buf();
        assert!(path.is_file());

        assert!(session.delete().unwrap(), "a saved session had a file to remove");
        assert!(!path.exists(), "the file is gone");

        // A write after the delete must not bring the file back: the user asked for it to
        // be gone, and a late append would quietly recreate it.
        let after = session.push_message(Message::user_text("late"), None, None);
        assert!(matches!(after, Err(SessionError::Deleted)), "{after:?}");
        assert!(!path.exists(), "a refused write must not recreate the file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_the_last_session_takes_its_store_directory_with_it() {
        // End to end through the store: create, say something, delete. The session's
        // directory and its row in the table are both gone afterwards.
        let root = std::env::temp_dir().join(format!("pidel{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let mut session = Session::create_in(&crate::config::sessions_dir_in(&store, &project), &project, "work/m")
            .unwrap();
        session.register_under = Some(store.clone());
        session.push_message(Message::user_text("你好"), None, None).unwrap();
        let dir = session.path().parent().unwrap().to_path_buf();
        assert!(dir.is_dir());

        session.delete().unwrap();
        assert!(crate::config::forget_dir_if_empty_in(&store, &project));
        assert!(!dir.exists());
        assert!(list_in(&store).is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_deleted_session_disappears_from_the_resume_list() {
        let (mut session, dir) = temp_session_with_a_message("delete-list");
        session.set_name(Some("要删掉的会话")).unwrap();
        let path = session.path().to_path_buf();
        assert_eq!(list_in(&dir).len(), 1);

        session.delete().unwrap();
        assert!(list_in(&dir).is_empty(), "the deleted session is no longer offered");
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_header_is_the_first_line_and_is_never_rewritten() {
        let (mut session, dir) = temp_session_with_a_message("header");
        let before = std::fs::read_to_string(session.path()).unwrap();
        session.set_name(Some("我的会话")).unwrap();
        session.set_name(Some("改个名字")).unwrap();
        let after = std::fs::read_to_string(session.path()).unwrap();
        assert!(after.starts_with(&before), "the first line must not change");
        assert_eq!(session.name().as_deref(), Some("改个名字"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_name_clears_the_session_name() {
        let (mut session, dir) = temp_session("clear");
        session.set_name(Some("x")).unwrap();
        session.set_name(None).unwrap();
        assert_eq!(session.name(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reopening_replays_the_conversation() {
        let (mut session, dir) = temp_session("replay");
        let path = session.path().to_path_buf();
        session.push_message(Message::user_text("hello"), None, None).unwrap();
        session
            .push_message(
                Message::assistant_text("hi there"),
                Some(Usage { input: 10, output: 3, cache_read: 0, cache_write: 0 }),
                Some(StopReason::Stop),
            )
            .unwrap();
        session.push_turn_context(&dir, "work/m", "high").unwrap();
        drop(session);

        let reopened = Session::open(&path).unwrap();
        let messages = reopened.context_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[1].text(), "hi there");
        // The turn context is stored but is not part of the conversation.
        assert!(reopened.records().iter().any(|r| matches!(r, Record::TurnContext { .. })));
        assert_eq!(reopened.totals.input, 10);
        assert_eq!(reopened.last_usage.unwrap().output, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_newest_turn_context_names_the_model_in_use() {
        // `/model` changes the model for the rest of the conversation. The header is written
        // once, at creation, and never rewritten — so remembering the choice means reading it
        // back from the turn contexts, and the newest one is the answer.
        let (mut session, dir) = temp_session("model-switch");
        let path = session.path().to_path_buf();
        session.push_message(Message::user_text("hello"), None, None).unwrap();
        session.push_turn_context(&dir, "work/first", "low").unwrap();
        session.push_message(Message::user_text("again"), None, None).unwrap();
        session.push_turn_context(&dir, "work/second", "max").unwrap();
        drop(session);

        let reopened = Session::open(&path).unwrap();
        assert_eq!(
            reopened.current_model(),
            Some(("work/second".to_string(), "max".to_string())),
            "the newest recorded turn wins"
        );
        // And it does not leak into the conversation.
        assert_eq!(reopened.context_messages().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_written_by_an_older_version_still_opens() {
        // The compatibility that has to keep working is files already on disk. Records carry
        // fields this version no longer writes — `format` on the header, the window chain on
        // a checkpoint — and they must be ignored rather than fatal: a stricter reader would
        // turn every existing session into "解析失败".
        let dir = std::env::temp_dir().join(format!("pi-oldfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","format":1,"id":"01a0c9b3-b78c-726b-9e74-fc987dcd42bd","timestamp":"2026-09-22T15:19:53Z","cwd":"/tmp","model":"work/m"}"#,
                "\n",
                r#"{"type":"response_item","parent_id":"a","id":"x1","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
                "\n",
                r#"{"type":"compacted","parent_id":"x1","id":"c1","window_id":"w1","previous_window_id":"w0","reason":"manual","summary":"s","replacement_history":[],"read_files":[],"modified_files":[],"timestamp":"2026-09-22T15:20:00Z"}"#,
                "\n",
            ),
        )
        .unwrap();

        let session = Session::open(&path).unwrap();
        assert_eq!(session.header().model, "work/m");
        // The checkpoint's (empty) replacement history is the context, and the user message
        // before it is still on disk as a record.
        assert!(session.records().iter().any(|r| matches!(r, Record::Compacted { .. })));
        assert_eq!(session.user_history(), vec!["hi".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_with_no_turn_context_has_no_recorded_model() {
        // Sessions written before turn contexts existed still have to open, and the caller
        // falls back to the header for them rather than getting an empty model.
        let (mut session, dir) = temp_session("no-turn-context");
        let path = session.path().to_path_buf();
        session.push_message(Message::user_text("hello"), None, None).unwrap();
        drop(session);

        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.current_model(), None);
        assert!(!reopened.header().model.is_empty(), "the header still names one");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn usage_recorded_before_a_checkpoint_is_not_reused_for_the_threshold() {
        let (mut session, dir) = temp_session("usage");
        session
            .push_message(
                Message::assistant_text("big"),
                Some(Usage { input: 900_000, output: 100, cache_read: 0, cache_write: 0 }),
                Some(StopReason::Stop),
            )
            .unwrap();
        assert_eq!(session.last_usage.unwrap().input, 900_000);
        session
            .push_compaction("manual", "summary", vec![Message::user_text("hello")], vec![], vec![], None)
            .unwrap();
        assert!(session.last_usage.is_none());
        assert_eq!(session.context_messages().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn context_messages_come_from_the_last_checkpoint() {
        let (mut session, dir) = temp_session("checkpoint");
        session.push_message(Message::user_text("old one"), None, None).unwrap();
        session.push_message(Message::assistant_text("old answer"), None, None).unwrap();
        session
            .push_compaction(
                "threshold",
                "summary text",
                vec![Message::user_text("old one")],
                vec!["a.rs".into()],
                vec![],
                None,
            )
            .unwrap();
        session.push_message(Message::user_text("new question"), None, None).unwrap();

        let messages = session.context_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "old one");
        assert_eq!(messages[1].text(), "new question");
        // The summary itself is a checkpoint field, not a message in the history.
        assert!(messages.iter().all(|m| !m.text().contains("summary text")));

        // Reopening must produce exactly the same view.
        let path = session.path().to_path_buf();
        drop(session);
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.context_messages().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_the_failed_assistant_survives_a_reopen() {
        let (mut session, dir) = temp_session("drop");
        session.push_message(Message::user_text("question"), None, None).unwrap();
        session
            .push_message(
                Message::Assistant { content: vec![], stop_reason: Some(StopReason::Error) },
                Some(Usage { input: 999_999, output: 0, cache_read: 0, cache_write: 0 }),
                Some(StopReason::Error),
            )
            .unwrap();
        assert_eq!(session.context_messages().len(), 2);
        session.drop_last_assistant().unwrap();
        let messages = session.context_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "question");
        // The stale usage from the discarded response must not drive the threshold.
        assert!(session.last_usage.is_none());
        let path = session.path().to_path_buf();
        drop(session);
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.context_messages().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let stamp = now();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.ends_with('Z'));
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[10..11], "T");
        // 1 700 000 000 = 2023-11-14T22:13:20Z
        assert_eq!(civil_from_unix(1_700_000_000), (2023, 11, 14, 22, 13, 20));
    }

    #[test]
    fn the_resume_list_skips_the_environment_block() {
        // The environment block is a user message in the file, so treating every user
        // message as the headline would show `<environment> 工作目录: …` instead of what the
        // session was actually about.
        let (mut session, dir) = temp_session("resume-snippet");
        session
            .push_message(Message::user_text("帮我重构 config.rs"), None, None)
            .unwrap();
        session
            .push_message(Message::assistant_text("好，我先读一下"), None, None)
            .unwrap();
        // Session::create writes only the header; the environment block belongs to
        // `Agent::new`, so these two pushes are the whole conversation.
        assert_eq!(session.context_messages().len(), 2);

        let summaries = list_in(&dir);
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.messages, 2, "the environment block must not be counted");
        assert_eq!(summary.snippet, "帮我重构 config.rs", "{}", summary.snippet);
        assert!(!summary.snippet.contains("<environment>"));
        assert_eq!(summary.label(40), "帮我重构 config.rs");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sessions_are_stored_under_the_directory_they_belong_to() {
        // A conversation belongs to the project it happened in. Listing every other
        // project's history buries the relevant ones, and resuming the wrong project's
        // conversation would run its commands against the wrong tree.
        let root = std::env::temp_dir().join(format!("piscope{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        // The table lives beside the sessions it names, and only gains a directory when one
        // of them actually stores something (see the registration tests below).
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let dir_a = crate::config::sessions_dir_in(&store, &a);
        let dir_b = crate::config::sessions_dir_in(&store, &b);
        let mut in_a = Session::create_in(&dir_a, &a, "work/m").unwrap();
        in_a.push_message(Message::user_text("在 a 里"), None, None).unwrap();

        // The two directories are separate stores, so `b` cannot see `a`'s session.
        assert_eq!(list_in(&dir_a).len(), 1);
        assert!(list_in(&dir_b).is_empty());
        // And the id, if it does not match, is an error rather than a guess.
        assert!(find_by_prefix_in(&dir_b, in_a.id()).is_err());
        assert!(find_by_prefix_in(&dir_a, in_a.id()).is_ok());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_same_directory_always_gets_the_same_id() {
        // The id names a directory. Minting a new one per call would scatter one project's
        // sessions across the store, and nothing would be able to find them again.
        let root = std::env::temp_dir().join(format!("pisdir{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let first = crate::config::register_dir_in(&store, &project);
        assert_eq!(crate::config::register_dir_in(&store, &project), first);
        assert_eq!(crate::config::dir_id_in(&store, &project).as_deref(), Some(first.as_str()));
        assert_eq!(
            crate::config::sessions_dir_in(&store, &project).file_name().unwrap(),
            first.as_str()
        );
        // A table written here is readable by the next process, which is the whole point.
        let index = crate::config::read_dirs_index_for_test(&store);
        assert_eq!(index.get(&first).map(String::as_str), Some(project.to_string_lossy().as_ref()));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn merely_looking_at_a_directory_does_not_register_it() {
        // The table says where sessions are, so it must not become a log of everywhere pi
        // has been run. A directory that never produced a session stays out of it, and the
        // id it would have used is not reserved either.
        let root = std::env::temp_dir().join(format!("pilook{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();

        assert!(crate::config::dir_id_in(&store, &project).is_none());
        // Asking twice gives two ids, which is fine: neither names a real directory.
        let _ = crate::config::sessions_dir_in(&store, &project);
        let _ = crate::config::sessions_dir_in(&store, &project);
        assert!(crate::config::read_dirs_index_for_test(&store).is_empty());
        assert!(crate::config::dir_id_in(&store, &project).is_none());

        // Creating a session is what registers it, and then the id is stable.
        let id = crate::config::register_dir_in(&store, &project);
        assert_eq!(crate::config::dir_id_in(&store, &project).as_deref(), Some(id.as_str()));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_empty_session_never_reaches_the_disk() {
        let (mut session, dir) = temp_session("empty");
        // Starting pi and leaving must not add a session to the list, so nothing is
        // written until there is something to remember.
        assert!(!session.path().exists(), "no file before the first message");
        assert!(list_in(&dir).is_empty());
        assert!(!session.is_saved());

        // The environment block alone does not count: it is bookkeeping every session
        // starts with, not a conversation.
        session
            .push_message(
                Message::user_text("<environment>\n工作目录: /tmp\n</environment>"),
                None,
                None,
            )
            .unwrap();
        assert!(!session.path().exists(), "the environment block alone is not a session");
        assert!(list_in(&dir).is_empty());

        // The first real message brings the whole beginning with it, in order, so the file
        // is exactly what it would have been had it been written from the start.
        session
            .push_message(Message::user_text("你好"), None, None)
            .unwrap();
        assert!(session.is_saved());
        assert!(session.path().is_file());
        let text = std::fs::read_to_string(session.path()).unwrap();
        let kinds: Vec<&str> = text
            .lines()
            .map(|line| {
                if line.contains("session_meta") {
                    "header"
                } else if line.contains("<environment>") {
                    "env"
                } else {
                    "said"
                }
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["header", "env", "said"],
            "the buffered records are flushed in order, header first"
        );
        assert_eq!(list_in(&dir).len(), 1);
        // Reopening sees the same conversation, header included.
        let reopened = Session::open(session.path()).unwrap();
        assert_eq!(reopened.id(), session.id());
        assert_eq!(reopened.context_messages().len(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resuming_elsewhere_moves_the_recorded_directory() {
        // The header records where the session *started*; the newest environment block says
        // where it is happening now. Resuming in another directory must append a block and
        // update the header, so a second resume does not think it moved again.
        let (mut session, dir) = temp_session("relocate");
        let elsewhere = std::env::temp_dir().join("pi-relocate-target");
        std::fs::create_dir_all(&elsewhere).unwrap();

        assert!(session.current_cwd().is_none(), "no block yet");
        session.relocate(&elsewhere).unwrap();
        assert_eq!(session.header().cwd, elsewhere.to_string_lossy());
        // Only the header moved; there is still no conversation, so no environment block.
        assert!(session.current_cwd().is_none());

        let block = crate::agent::r#loop::environment_block(&elsewhere, "sid", "/usr/bin/zsh");
        session.push_message(Message::user_text(block), None, None).unwrap();
        assert_eq!(session.current_cwd().as_deref(), Some(elsewhere.as_path()));

        // Relocating to the same place is a no-op.
        let before = session.records().len();
        session.relocate(&elsewhere).unwrap();
        assert_eq!(session.records().len(), before);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn current_cwd_takes_the_newest_block() {
        let (mut session, dir) = temp_session("cwd-blocks");
        for cwd in ["/tmp/one", "/tmp/two", "/tmp/three"] {
            let block = crate::agent::r#loop::environment_block(Path::new(cwd), "sid", "zsh");
            session.push_message(Message::user_text(block), None, None).unwrap();
        }
        assert_eq!(
            session.current_cwd().as_deref(),
            Some(Path::new("/tmp/three")),
            "the newest block wins"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn previews_are_single_line_and_bounded() {
        let message = Message::user_text("line one\nline two");
        assert_eq!(message_preview(&message, 100), "line one line two");
        assert!(crate::util::width(&message_preview(&message, 6)) <= 6);
    }
}
