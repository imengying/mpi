//! Disk-backed output capture: children never block on unread pipes and memory is bounded.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::util::{self, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES};

pub(super) struct Capture {
    pub file: std::fs::File,
    path: PathBuf,
    retained: bool,
}

impl Capture {
    pub fn new() -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("pi-{}.log", uuid::Uuid::now_v7()));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok(Self {
            file,
            path,
            retained: false,
        })
    }

    pub fn finish(mut self, sanitize: bool) -> std::io::Result<(String, Option<PathBuf>)> {
        let size = self.file.metadata()?.len();
        let start = size.saturating_sub(MAX_OUTPUT_BYTES as u64);
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(MAX_OUTPUT_BYTES);
        Read::by_ref(&mut self.file)
            .take(MAX_OUTPUT_BYTES as u64)
            .read_to_end(&mut bytes)?;
        // The seek may land inside a UTF-8 character. Skip only continuation bytes.
        let offset = if start > 0 {
            bytes.iter().take_while(|b| **b & 0xc0 == 0x80).count()
        } else {
            0
        };
        let decoded = String::from_utf8_lossy(&bytes[offset..]);
        let text = if sanitize {
            util::sanitize(&decoded)
        } else {
            decoded.into_owned()
        };
        let text = text.trim_end_matches('\n');
        let truncated =
            start > 0 || text.len() > MAX_OUTPUT_BYTES || text.lines().count() > MAX_OUTPUT_LINES;
        if !truncated {
            return Ok((text.to_string(), None));
        }
        // Leave room for the path and exit status so the shared budget does not truncate again.
        let mut cut = text.len().saturating_sub(MAX_OUTPUT_BYTES - 1024);
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        let tail = &text[cut..];
        let tail = if start > 0 || cut > 0 {
            tail.split_once('\n')
                .map(|(_, rest)| rest)
                .filter(|s| !s.is_empty())
                .unwrap_or(tail)
        } else {
            tail
        };
        let lines: Vec<_> = tail.lines().collect();
        let kept = lines[lines.len().saturating_sub(MAX_OUTPUT_LINES - 4)..].join("\n");
        self.retained = true;
        Ok((
            format!(
                "{kept}\n\n[输出过长，已截断。完整输出：{}]",
                self.path.display()
            ),
            Some(self.path.clone()),
        ))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.retained {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Read a small delimiter-separated field without allocating an unbounded record.
pub(super) fn field(
    reader: &mut impl BufRead,
    delimiter: u8,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(std::io::Error::other("输出记录不完整"))
            };
        }
        let end = buffer.iter().position(|b| *b == delimiter);
        let count = end.unwrap_or(buffer.len());
        if bytes.len().saturating_add(count) > limit {
            return Err(std::io::Error::other("输出字段过长"));
        }
        bytes.extend_from_slice(&buffer[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(Some(bytes));
        }
    }
}

/// Copy one possibly huge line in fixed-size chunks, including its newline.
pub(super) fn copy_line(reader: &mut impl BufRead, writer: &mut impl Write) -> std::io::Result<()> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        let end = buffer.iter().position(|b| *b == b'\n');
        let count = end.map_or(buffer.len(), |index| index + 1);
        writer.write_all(&buffer[..count])?;
        reader.consume(count);
        if end.is_some() {
            return Ok(());
        }
    }
}
