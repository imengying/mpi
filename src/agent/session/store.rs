//! The session store: where session files live, and how they are listed and found.
//!
//! Sessions are grouped by the directory the work happened in, so `/resume` shows the
//! current project's conversations and nothing else. The mapping from a path to its group
//! lives in `dirs.json`; `config` owns that table, and this module is the reader on top of
//! it — listing, previewing and resolving an id prefix to a file.
//!
//! Recovery parsing is shared with [`super::Session::open`], which is why the parse here is
//! read-only: listing must never lock or repair a file somebody is writing.

use std::path::{Path, PathBuf};

use super::dirs::sessions_dir;
use crate::llm::{Block, Message};

use super::{Record, Session};

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
    let (hour, minute, second) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
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
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut summaries: Vec<SessionSummary> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let Ok((session, ..)) = Session::snapshot(&path, &file) else {
            continue;
        };
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
        Message::User { .. } => {
            crate::util::truncate(&crate::util::one_line(&message.text()), width, "…")
        }
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
        let named = SessionSummary {
            name: Some("我的会话".into()),
            ..summary
        };
        assert_eq!(named.label(40), "我的会话");
    }
}
