//! The terminal as a device: raw mode, the window title, and putting both back.
//!
//! Nothing here knows what is on screen. It is the layer that owns the terminal itself —
//! which is why raw mode is counted rather than toggled, and why the title is stripped with
//! the same care it is set.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crossterm::{cursor, terminal};

use crate::util;

/// The name the window title leads with, as pi uses `π`.
pub const APP_TITLE: &str = "π";

/// Raw mode, held by every part of the UI that needs it.
///
/// The count is the whole point. The prompt needs raw mode; so do the pickers and the
/// authorization panel, and those run *inside* a turn. With a plain guard each of them would
/// switch raw mode off on the way out, and the prompt would then be reading a terminal that
/// echoes and line-buffers — which is why typing during a turn used to go nowhere: the bytes
/// were swallowed by the line discipline until the next prompt asked for a line.
///
/// So the mode belongs to the program, not to the read: it goes on at the first request and
/// off when the process tears down. The count exists so the nesting is not a lie.
pub struct RawGuard;

static RAW_HOLDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl RawGuard {
    pub(crate) fn enter() -> std::io::Result<Self> {
        if RAW_HOLDERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            && let Err(err) = terminal::enable_raw_mode() {
                RAW_HOLDERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(err);
            }
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // The mode is not switched off here: see `RawGuard` above. It is released once, at
        // teardown, so a picker closing does not take the prompt's raw mode with it.
        RAW_HOLDERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Leave the terminal clean when pi exits. Piped output gets no escape sequences.
pub fn teardown() {
    let _ = terminal::disable_raw_mode();
    if !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, cursor::Show);
    let _ = out.flush();
}

/// The window title: `π - <session name> - <project>`, or `π - <project>` when the session
/// has no name yet.
///
/// The project is the **name** of its root directory, not the path it lives at. A terminal
/// tab is a few centimetres wide, and `π - ~/文档/mpi` spends most of them on a location the
/// user already knows — they are looking for which project, not where it is. A session name
/// still takes precedence: it is what the user chose to call this conversation.
///
/// Control characters are flattened: a session name is user input, and a newline or an
/// escape byte in it would break out of the OSC sequence and let the rest be interpreted
/// The window title: `π - <session name> - <project>`, or `π - <project>` when the session
/// has no name yet.
///
/// The project is the **name** of its root directory, not the path it lives at. A terminal
/// tab is a few centimetres wide, and `π - ~/文档/mpi` spends most of them on a location the
/// user already knows — they are looking for which project, not where it is. A session name
/// still takes precedence: it is what the user chose to call this conversation.
///
/// Control characters are flattened: a session name is user input, and a newline or an
/// escape byte in it would break out of the OSC sequence and let the rest be interpreted
/// as terminal commands.
pub fn window_title(name: Option<&str>, cwd: &Path) -> String {
    let mut title = String::from(APP_TITLE);
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    if let Some(name) = name {
        title.push_str(" - ");
        title.push_str(&util::one_line(name));
    } else if let Some(project) = project_label(cwd) {
        title.push_str(" - ");
        title.push_str(&project);
    }
    title
}

/// The project's name: the last component of the directory the work happens in.
///
/// Taken from the directory itself rather than from the git root so the title matches what
/// the user typed to get here. The file-system root has no name, so it yields `None` and
/// the title stays just `π` rather than becoming `π - /`.
fn project_label(cwd: &Path) -> Option<String> {
    let name = cwd.file_name()?.to_string_lossy().to_string();
    let name = util::one_line(&name);
    (!name.is_empty()).then_some(name)
}

/// Strip the window title on the way out, leaving the terminal as it was found.
pub fn clear_title() {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = write!(out, "\u{1b}]0;\u{7}");
    let _ = out.flush();
}
