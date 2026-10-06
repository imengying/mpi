//! The file behind a session: locking, appending, and recovering bytes.
//!
//! Every session is one JSONL file, and this is the only module that touches its descriptor.
//! Two rules shape the code here:
//!
//! * **The lock is the session.** A second writer is refused rather than merged, so the
//!   record order in the file is the order the appends happened in.
//! * **A broken tail is repaired, interior damage is not.** A write cut off mid-record is
//!   what a kill during an append looks like, and losing that one line is correct. Damage
//!   anywhere else means the file is not what it claims to be, and guessing would be worse
//!   than reporting it.

use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::config::Usage;
use crate::llm::Message;

use super::{Record, Session};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("无法读写会话文件：{0}")]
    Io(#[from] std::io::Error),
    #[error("会话文件格式无法识别：{0}")]
    Parse(String),
    #[error("会话已被删除")]
    Deleted,
}

pub(crate) struct SessionFile(std::fs::File);

impl SessionFile {
    fn lock(file: std::fs::File) -> std::io::Result<Self> {
        file.try_lock()
            .map_err(|err| std::io::Error::other(format!("会话正在使用或无法加锁：{err}")))?;
        Ok(Self(file))
    }
}

impl std::ops::Deref for SessionFile {
    type Target = std::fs::File;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SessionFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        // A concurrent fork may inherit the descriptor until exec. Explicitly unlocking
        // releases our writer lock even while that temporary duplicate is still alive.
        let _ = self.0.unlock();
    }
}

/// Where a session's records live: buffered, saved, or permanently deleted.
pub(crate) enum Storage {
    /// No file yet. Records are held in memory until the session becomes a conversation.
    Buffered,
    /// The file exists and is open for appending.
    Open(SessionFile),
    /// The file is gone. Nothing may create it again.
    Deleted,
}

impl Session {
    /// Open an existing session file and replay it.
    pub fn open(path: &Path) -> Result<Self, SessionError> {
        let mut file = SessionFile::lock(
            std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)?,
        )?;
        let (mut session, valid_len, partial_tail, needs_newline) = Self::snapshot(path, &file)?;
        let pending = crate::llm::pending_tool_calls(&session.context_messages())
            .map_err(|err| SessionError::Parse(err.to_string()))?;
        if partial_tail {
            file.set_len(valid_len)?;
            file.sync_data()?;
            session
                .recovery_notes
                .push("末尾未写完整的记录已移除。".into());
        } else if needs_newline {
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        session.storage = Storage::Open(file);
        if !pending.is_empty() {
            // The facts and nothing else: the file stopped mid-call, so what the call did is
            // unknown, and that is the one thing the user has to settle before working on.
            session
                .recovery_notes
                .push("上次会话中途退出，有工具调用未记录结果；请先检查实际状态。".into());
        }
        for (tool_call_id, name) in pending {
            session.push_message(Message::Tool {
                tool_call_id,
                name,
                status: crate::llm::ToolStatus::Unknown,
                content: "上次会话意外中断，此工具调用的执行结果未知。请先检查文件或实际状态，再决定是否重试。".into(),
            }, None, None)?;
        }
        Ok(session)
    }

    /// Read-only listing shares recovery parsing, but never locks or repairs a live file.
    pub(super) fn snapshot(
        path: &Path,
        file: &std::fs::File,
    ) -> Result<(Self, u64, bool, bool), SessionError> {
        let mut reader = BufReader::new(file.try_clone()?);
        reader.seek(SeekFrom::Start(0))?;
        let mut records: Vec<Record> = Vec::new();
        let mut line = Vec::new();
        let mut number = 0;
        let mut valid_len = 0u64;
        let mut partial_tail = false;
        let mut needs_newline = false;
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            number += 1;
            let terminated = line.ends_with(b"\n");
            if line.iter().all(u8::is_ascii_whitespace) {
                valid_len += line.len() as u64;
                needs_newline = !terminated;
                continue;
            }
            match serde_json::from_slice::<Record>(&line) {
                Ok(record) => {
                    record.validate().map_err(|err| {
                        SessionError::Parse(format!("{} 第 {number} 行：{err}", path.display()))
                    })?;
                    records.push(record);
                    valid_len += line.len() as u64;
                    needs_newline = !terminated;
                }
                Err(err)
                    if !terminated
                        && !records.is_empty()
                        && (err.is_eof()
                            || std::str::from_utf8(&line).is_err_and(|e| {
                                e.error_len().is_none()
                                    && serde_json::from_slice::<Record>(&line[..e.valid_up_to()])
                                        .is_err_and(|err| err.is_eof())
                            })) =>
                {
                    partial_tail = true;
                    break;
                }
                Err(err) => {
                    return Err(SessionError::Parse(format!(
                        "{} 第 {number} 行：{err}",
                        path.display()
                    )));
                }
            }
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
            register_under: path
                .starts_with(super::dirs::sessions_root())
                .then(super::dirs::sessions_root),
            storage: Storage::Buffered,
            last_id,
            records,
            totals: Usage::default(),
            last_usage: None,
            last_usage_index: None,
            recovery_notes: Vec::new(),
        };
        session.cwd = session
            .current_cwd()
            .unwrap_or_else(|| PathBuf::from(&session.header.cwd));
        session.recompute_usage();
        Ok((session, valid_len, partial_tail, needs_newline))
    }

    pub(super) fn append(&mut self, record: Record, id: String) -> Result<(), SessionError> {
        record
            .validate()
            .map_err(|err| SessionError::Parse(err.into()))?;
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
                let mut line = serde_json::to_vec(&record)
                    .map_err(|err| SessionError::Parse(err.to_string()))?;
                line.push(b'\n');
                file.write_all(&line)?;
                file.sync_data()?;
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
    pub(super) fn persist(&mut self) -> Result<(), SessionError> {
        // The directory is registered now, because this is the moment the store gains
        // something to name. `create` only had a provisional id: registering there would
        // record every directory pi is ever started in, and the table is meant to answer
        // "where are the sessions", not "where has the user been".
        //
        // The registered id can differ from the provisional one, so the path is rebuilt from
        // it rather than reused. Both name the same directory; only the name changes, and it
        // changes before the file exists, so nothing has to be moved.
        if let Some(root) = self.register_under.clone() {
            let registered = super::dirs::register_dir_in(&root, &self.cwd);
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
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = SessionFile::lock(options.open(&self.path)?)?;
        for record in &self.records {
            let line = serde_json::to_string(record)
                .map_err(|err| SessionError::Parse(err.to_string()))?;
            writeln!(file, "{line}")?;
        }
        file.sync_data()?;
        self.storage = Storage::Open(file);
        Ok(())
    }

    /// Delete the session file.
    ///
    /// The handle is closed first: an open handle keeps the file alive and, on Windows,
    /// blocks the unlink outright. After this the session is inert — `Session::append`
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
}
