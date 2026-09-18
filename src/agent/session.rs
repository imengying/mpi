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

pub const FORMAT: u32 = 1;

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
        format: u32,
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
        window_id: String,
        previous_window_id: Option<String>,
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

    pub fn usage(&self) -> Option<Usage> {
        match self {
            Record::ResponseItem { usage, .. }
            | Record::EventMsg { usage, .. }
            | Record::Compacted { usage, .. } => *usage,
            _ => None,
        }
    }
}

/// An open session file, plus the bookkeeping needed to append safely.
pub struct Session {
    header: SessionHeader,
    path: PathBuf,
    file: std::fs::File,
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
}

impl Session {
    /// Start a new session in the default session directory.
    pub fn create(cwd: &Path, model: &str) -> Result<Self, SessionError> {
        Self::create_in(&sessions_dir(), cwd, model)
    }

    /// Start a new session under `dir`. The directory is a parameter so tests do not have
    /// to mutate process-wide environment variables.
    pub fn create_in(dir: &Path, cwd: &Path, model: &str) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir)?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = dir.join(format!("{id}.jsonl"));
        let header = SessionHeader {
            id: id.clone(),
            timestamp: now(),
            cwd: cwd.to_string_lossy().to_string(),
            model: model.to_string(),
        };
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let record = Record::SessionMeta { format: FORMAT, header: header.clone() };
        writeln!(file, "{}", serde_json::to_string(&record).unwrap())?;
        file.flush()?;
        Ok(Session {
            header,
            path,
            file,
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
        let mut session = Session {
            header,
            path: path.to_path_buf(),
            file: std::fs::OpenOptions::new().append(true).open(path)?,
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
        let previous_window = self
            .records
            .iter()
            .rev()
            .find_map(|record| match record {
                Record::Compacted { window_id, .. } => Some(window_id.clone()),
                _ => None,
            });
        let id = self.next_id();
        let record = Record::Compacted {
            parent_id: self.last_id.clone(),
            id: id.clone(),
            window_id: uuid::Uuid::now_v7().to_string(),
            previous_window_id: previous_window,
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

    fn append(&mut self, record: Record, id: String) -> Result<(), SessionError> {
        let line = serde_json::to_string(&record)
            .map_err(|err| SessionError::Parse(err.to_string()))?;
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        if let Some(usage) = record.usage() {
            self.totals.add(&usage);
        }
        self.last_id = Some(id);
        self.records.push(record);
        Ok(())
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
        if let Some(checkpoint) = self.last_checkpoint_index() {
            if last_index.map(|index| index < checkpoint).unwrap_or(true) {
                last_usage = None;
                last_index = None;
            }
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
pub fn list() -> Vec<SessionSummary> {
    list_in(&sessions_dir())
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
        let messages = session
            .records()
            .iter()
            .filter(|record| record.message().is_some())
            .count();
        let snippet = session
            .records()
            .iter()
            .find_map(|record| match record.message() {
                Some(Message::User { .. }) => Some(record.message().unwrap().text()),
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
    summaries.sort_by(|a, b| b.modified.cmp(&a.modified));
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
        let dir = std::env::temp_dir().join(format!("mpi-session-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session::create_in(&dir, &dir, "work/m").unwrap();
        (session, dir)
    }

    #[test]
    fn the_header_is_the_first_line_and_is_never_rewritten() {
        let (mut session, dir) = temp_session("header");
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
    fn previews_are_single_line_and_bounded() {
        let message = Message::user_text("line one\nline two");
        assert_eq!(message_preview(&message, 100), "line one line two");
        assert!(crate::util::width(&message_preview(&message, 6)) <= 6);
    }
}
