//! The session store's directory table: `~/.pi/sessions` and its `dirs.json`.
//!
//! Sessions are grouped by where the work happened, and the group is named by a short id
//! rather than by the path. Encoding the path into the name produced names like
//! `--home-user-文档-mpi--`: long enough to wrap in a listing, and still not the path it
//! stands for. An id is short and sortable, and this table is the one place that knows where
//! a session came from — which is also what makes a *rename* of the project a non-event.
//!
//! This lives here rather than in `config` because it is not configuration: the config file
//! is read once at start-up and describes how pi behaves, while this is a mutable table of
//! where sessions *are*, written on the first message of a session and pruned when the last
//! one goes. Nothing but `agent::session` reads it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::config::{Defaults, home_dir};

/// The root of the session store: `~/.pi/sessions`.
pub fn sessions_root() -> PathBuf {
    home_dir().join(Defaults::SESSIONS_DIR)
}

/// The file mapping a short directory id back to the path it stands for.
pub fn dirs_index_path() -> PathBuf {
    dirs_index_path_in(&sessions_root())
}

/// [`dirs_index_path`] under a given store, so tests do not touch the real table.
fn dirs_index_path_in(root: &Path) -> PathBuf {
    root.join("dirs.json")
}

/// The directory holding the sessions of one working directory.
///
/// Sessions are grouped by where the work happened: a session is about one project, and
/// listing every conversation the user ever had — in every other directory — buries the
/// ones that belong to the project in front of them. `/resume` therefore shows the current
/// directory's sessions and nothing else.
///
/// The directory is named by a short id rather than by the path, and the mapping lives in
/// `dirs.json`. Encoding the path into the name produced names like
/// `--home-user-文档-mpi--`: long enough to wrap in a listing, and still not the path it
/// stands for. An id is short and sortable, and the table is the one place that knows where
/// a session came from — which is also what makes a *rename* of the project a non-event.
pub fn sessions_dir(cwd: &Path) -> PathBuf {
    sessions_dir_in(&sessions_root(), cwd)
}

/// [`sessions_dir`] under a given store, so tests do not touch the real one.
pub fn sessions_dir_in(root: &Path, cwd: &Path) -> PathBuf {
    root.join(match dir_id_in(root, cwd) {
        Some(id) => id,
        // Nothing recorded: the answer is an id that names no directory, which is exactly
        // right — there is nothing to list. It is *not* written here, because merely asking
        // where a directory's sessions would live must not add it to the table.
        None => provisional_dir_id(),
    })
}

/// An id for a directory that has no sessions yet.
///
/// Never written: the table is a list of directories that *have* sessions, and registering
/// every directory pi is merely run in would make it a log of where the user has been.
fn provisional_dir_id() -> String {
    short_id(&Uuid::now_v7().simple().to_string())
}

/// Register `cwd` and return the id its sessions go under, minting one if it is new.
///
/// Called when a session is created, so the table only ever gains directories that produced
/// something.
pub fn register_dir(cwd: &Path) -> String {
    register_dir_in(&sessions_root(), cwd)
}

/// [`register_dir`] under a given store, so tests do not touch the real one.
pub fn register_dir_in(root: &Path, cwd: &Path) -> String {
    if let Some(id) = dir_id_in(root, cwd) {
        return id;
    }
    // Not registered yet: take the lock, re-read (another process may have just added it),
    // and write only if it is still missing.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("dirs.lock"))
        .ok()
        .and_then(|handle| {
            // A failure to lock is not fatal: the write below is atomic, so the worst case is
            // a lost update, which the re-read on the next start repairs.
            handle.lock().ok().map(|()| handle)
        });
    let mut index = read_dirs_index(root);
    if let Some(id) = lookup_dir(&index, cwd) {
        return id;
    }
    let id = fresh_dir_id(&index);
    index.insert(id.clone(), cwd.to_string_lossy().to_string());
    let _ = write_dirs_index(root, &index);
    drop(lock);
    id
}

/// The id recorded for `cwd`, if this directory has sessions.
pub fn dir_id_in(root: &Path, cwd: &Path) -> Option<String> {
    lookup_dir(&read_dirs_index(root), cwd)
}

/// The id recorded for `cwd`, if any. Matching is on the absolute path, so a symlinked
/// route to the same directory is a different project — resolving symlinks here would make
/// the same directory reachable under names that disagree with what the user typed.
fn lookup_dir(index: &BTreeMap<String, String>, cwd: &Path) -> Option<String> {
    let wanted = cwd.to_string_lossy();
    index
        .iter()
        .find(|(_, path)| path.as_str() == wanted)
        .map(|(id, _)| id.clone())
}

/// A short id not already in `index`.
///
/// A uuid rather than a counter: it costs no extra dependency (session ids already use one),
/// and an id that is never reused matters more than being compact — an id dropped from the
/// table must not be handed to a different project later, which would silently point old
/// sessions at a new directory.
///
/// The **random tail** is what gets shortened, not the head. A v7 uuid leads with a
/// millisecond timestamp, so its first characters are identical for everything created
/// within the same ~65-second window; taking those would collide on every directory made in
/// a burst, and the retry loop below would never find a free one.
fn fresh_dir_id(index: &BTreeMap<String, String>) -> String {
    loop {
        let short = short_id(&Uuid::now_v7().simple().to_string());
        if !index.contains_key(&short) {
            return short;
        }
    }
}

/// The last 8 hex characters of an id: the random tail, never the timestamp head.
fn short_id(full: &str) -> String {
    full[full.len() - 8..].to_string()
}

fn read_dirs_index(root: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(dirs_index_path_in(root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write the table through a temporary file and a rename.
///
/// `rename` replaces the target in one step, so a reader never sees a half-written table
/// and a crash mid-write cannot destroy the mapping for every directory at once.
fn write_dirs_index(root: &Path, index: &BTreeMap<String, String>) -> std::io::Result<()> {
    let path = dirs_index_path_in(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(index)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    text.push('\n');
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path)
}

/// Drop `cwd`'s entry if its directory holds no sessions, and remove the directory.
///
/// Called after a session is deleted. Without it the store keeps a directory for every
/// project that was ever used, and `ls ~/.pi/sessions` stops being a list of the projects
/// that have history — which is the one thing the short-id layout is for.
///
/// Only an *empty* directory is dropped: the entry names a real store as long as one session
/// remains, and removing it then would orphan that session. The directory is removed before
/// the entry, so a crash between the two leaves an empty directory with no entry — harmless,
/// and the next session there registers a fresh id.
pub fn forget_dir_if_empty(cwd: &Path) -> bool {
    forget_dir_if_empty_in(&sessions_root(), cwd)
}

/// [`forget_dir_if_empty`] under a given store, so tests do not touch the real one.
pub fn forget_dir_if_empty_in(root: &Path, cwd: &Path) -> bool {
    let Some(id) = dir_id_in(root, cwd) else {
        return false;
    };
    let dir = root.join(&id);
    // A directory that cannot be read is treated as non-empty: forgetting an entry whose
    // sessions might still be there would make them unreachable, which is worse than a
    // stale row in a list nobody reads by hand.
    let Ok(mut entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    if entries.next().is_some() {
        return false;
    }
    let _ = std::fs::remove_dir(&dir);
    remove_dir_from_index(root, &id)
}

/// Remove one entry, under the same lock and atomic rewrite as registration.
fn remove_dir_from_index(root: &Path, id: &str) -> bool {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("dirs.lock"))
        .ok()
        .and_then(|handle| handle.lock().ok().map(|()| handle));
    let mut index = read_dirs_index(root);
    let removed = index.remove(id).is_some();
    if removed {
        let _ = write_dirs_index(root, &index);
    }
    drop(lock);
    removed
}

/// The table under a given store, for tests that assert on what was recorded.
pub fn read_dirs_index_for_test(root: &Path) -> BTreeMap<String, String> {
    read_dirs_index(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_of_new_directories_still_gets_distinct_ids() {
        // The id is shortened from a v7 uuid, whose first characters are a timestamp shared
        // by everything created in the same window. Shortening the *head* would make every
        // directory registered in one burst collide, and the retry loop would spin forever
        // looking for a free id. A handful of registrations in a row has to stay distinct.
        let ids: std::collections::HashSet<String> = (0..50)
            .map(|_| {
                let index = std::collections::BTreeMap::new();
                fresh_dir_id(&index)
            })
            .collect();
        assert_eq!(ids.len(), 50, "ids collided within one burst");
        for id in &ids {
            assert_eq!(id.len(), 8, "{id} is not a short id");
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id} is not hex");
        }
    }

    #[test]
    fn a_directory_leaves_the_table_once_its_last_session_is_gone() {
        // The table is a list of where history *is*. Keeping a row for every project ever
        // used would turn it into a log of where the user has been, which is exactly what
        // the short-id layout exists to avoid.
        let root = std::env::temp_dir().join(format!("piforget{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let id = register_dir_in(&store, &project);
        let dir = store.join(&id);
        std::fs::create_dir_all(&dir).unwrap();

        // A session is still there: the entry and the directory both stay.
        std::fs::write(dir.join("one.jsonl"), "{}").unwrap();
        assert!(!forget_dir_if_empty_in(&store, &project));
        assert!(dir.is_dir());
        assert!(dir_id_in(&store, &project).is_some());

        // The last one is gone: both go.
        std::fs::remove_file(dir.join("one.jsonl")).unwrap();
        assert!(forget_dir_if_empty_in(&store, &project));
        assert!(!dir.exists());
        assert!(
            dir_id_in(&store, &project).is_none(),
            "the id must not linger"
        );
        // Forgetting twice is not an error, just a no-op.
        assert!(!forget_dir_if_empty_in(&store, &project));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_store_sits_under_the_pi_directory() {
        // One directory for everything pi owns, so the file to edit sits next to the sessions
        // it produced. Asserted on the *names* rather than by calling the resolvers:
        // `sessions_dir` registers the directory it is asked about, and a test that calls it
        // writes into the real table — which is how a stray `/tmp/x` entry ended up in a
        // user's store.
        let home = home_dir();
        assert_eq!(sessions_root(), home.join("sessions"));
        assert_eq!(dirs_index_path(), home.join("sessions/dirs.json"));

        // Under a store of its own, the grouping still holds.
        let root = std::env::temp_dir().join(format!("pipaths{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = sessions_dir_in(&root, Path::new("/tmp/somewhere"));
        assert!(dir.starts_with(&root), "{}", dir.display());
        assert_eq!(dir.parent(), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(root);
    }
}
