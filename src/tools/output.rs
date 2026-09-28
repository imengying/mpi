//! Disk-backed output capture: children never block on unread pipes and memory is bounded.

use std::io::{Read, Seek, SeekFrom};
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
        self.file
            .by_ref()
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
