//! What `read` saw, so a later `edit` can tell "I typed it wrong" from "the file moved".
//!
//! A model edits from what it last read. Between that read and the edit the file may have
//! changed — the model itself wrote it, a build regenerated it, a formatting hook touched it.
//! Then `old_text` no longer matches, and the error a naive implementation gives is
//! "not found", which reads as *you typed it wrong*. The model's natural response is to retry
//! with a different guess, and it keeps guessing against content that is no longer there.
//!
//! Recording a fingerprint at read time turns that into a different, actionable sentence:
//! the file changed since you read it, read it again. Same underlying state, but it points
//! at the fix.
//!
//! The fingerprint is `stat` fields, not a content hash: this is a staleness hint, and
//! hashing every file a session reads would cost more than the mistake it prevents. A
//! same-size, same-mtime rewrite can slip through; that is the accepted limit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A file as it looked at the last read, reduced to what `stat` can cheaply report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: Option<std::time::SystemTime>,
}

impl Version {
    /// The fingerprint of the file at `path`, or `None` if it cannot be stat'ed.
    ///
    /// Modified time is compared as `SystemTime`, the exact value `stat` returned: a
    /// filesystem with coarse timestamps reports the same coarse value both times, so this
    /// never invents a difference.
    pub fn of(path: &Path) -> Option<Version> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(path).ok()?;
            Some(Version {
                dev: meta.dev(),
                ino: meta.ino(),
                size: meta.size(),
                mtime: meta.modified().ok(),
            })
        }
        #[cfg(not(unix))]
        {
            let meta = std::fs::metadata(path).ok()?;
            Some(Version {
                dev: 0,
                ino: 0,
                size: meta.len(),
                mtime: meta.modified().ok(),
            })
        }
    }
}

/// Per-batch record of what has been read. Shared by every tool call in the batch, so the
/// `read` of one call is visible to the `edit` of the next.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    seen: Arc<Mutex<HashMap<PathBuf, Version>>>,
}

impl Observed {
    /// Remember the state `read` just saw.
    pub fn note(&self, path: &Path) {
        let Some(version) = Version::of(path) else {
            return;
        };
        // A poisoned lock only means another tool panicked mid-update. Staleness detection is
        // an improvement, not a correctness requirement, so losing it beats failing the call.
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(path.to_path_buf(), version);
        }
    }

    /// What this batch last saw at `path`, if it was read at all.
    fn get(&self, path: &Path) -> Option<Version> {
        self.seen.lock().ok()?.get(path).copied()
    }

    /// Forget `path`, because the edit it just received changed the file.
    pub fn forget(&self, path: &Path) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.remove(path);
        }
    }

    /// Whether `path` is stale relative to the last read in this batch.
    ///
    /// Returns `Err` with the sentence the model should see when the file changed. A path
    /// never read in this batch is not stale: the model is allowed to edit what it has not
    /// read (long sessions, resumed sessions), and refusing that would break the common case
    /// to catch a rarer one.
    pub fn stale(&self, path: &Path, display: &str) -> Result<(), String> {
        let Some(was) = self.get(path) else {
            return Ok(());
        };
        let now = Version::of(path)
            .ok_or_else(|| format!("{display} 在读完之后被删掉了，重新看一下再改。"))?;
        if now == was {
            return Ok(());
        }
        Err(format!(
            "{display} 在读完之后被改过了；old_text 对不上不是拼写问题。重新读一遍再改。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untouched_read_file_is_not_stale() {
        let dir = crate::tools::temp_dir("observed");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let observed = Observed::default();
        observed.note(&file);
        assert!(observed.stale(&file, "a.txt").is_ok());
    }

    #[test]
    fn a_rewrite_between_read_and_edit_is_reported() {
        // The bug this catches: the model reads, the file changes, the model edits. A plain
        // "not found" sends it guessing at other spellings of text that is gone.
        let dir = crate::tools::temp_dir("observed");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let observed = Observed::default();
        observed.note(&file);
        std::fs::write(&file, "goodbye, and long enough to change the size\n").unwrap();
        let err = observed.stale(&file, "a.txt").unwrap_err();
        assert!(err.contains("被改过"), "{err}");
        assert!(err.contains("重新读"), "the fix has to be named: {err}");
    }

    #[test]
    fn a_deleted_file_says_deleted_not_changed() {
        let dir = crate::tools::temp_dir("observed");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        let observed = Observed::default();
        observed.note(&file);
        std::fs::remove_file(&file).unwrap();
        let err = observed.stale(&file, "a.txt").unwrap_err();
        assert!(err.contains("被删掉"), "{err}");
    }

    #[test]
    fn a_file_never_read_in_this_batch_is_editable() {
        // Long and resumed sessions edit files they read many turns ago, and the record does
        // not survive that far. Refusing these would break the common case.
        let dir = crate::tools::temp_dir("observed");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\n").unwrap();
        assert!(Observed::default().stale(&file, "a.txt").is_ok());
    }

    #[test]
    fn forgetting_after_a_write_keeps_the_next_edit_quiet() {
        // A batch may edit the same file twice. The second edit is based on the first, which
        // this batch performed, so it must not read as someone else's change.
        let dir = crate::tools::temp_dir("observed");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let observed = Observed::default();
        observed.note(&file);
        std::fs::write(&file, "two\n").unwrap();
        observed.forget(&file);
        assert!(observed.stale(&file, "a.txt").is_ok());
    }
}
