//! The terminal as a device: raw mode, the window title, and putting both back.
//!
//! Nothing here knows what is on screen. It is the layer that owns the terminal itself —
//! which is why raw mode is counted rather than toggled, and why the title is stripped with
//! the same care it is set.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crossterm::{cursor, event, terminal};

use crate::util;

/// The name the window title leads with, as pi uses `π`.
pub const APP_TITLE: &str = "π";

/// Raw mode belongs to the interactive session and ends at teardown.
/// Nested pickers and approval panels reuse it without changing its lifetime.
pub(crate) fn ensure_raw_mode() -> std::io::Result<()> {
    if terminal::is_raw_mode_enabled()? {
        return Ok(());
    }
    terminal::enable_raw_mode()?;
    let result = crossterm::execute!(
        std::io::stdout(),
        event::EnableBracketedPaste,
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    if result.is_err() {
        let _ = terminal::disable_raw_mode();
    }
    result
}

/// Leave the terminal clean when pi exits. Piped output gets no escape sequences.
pub fn teardown() {
    let active = terminal::is_raw_mode_enabled().unwrap_or(false);
    let _ = terminal::disable_raw_mode();
    if !active || !std::io::stdout().is_terminal() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(
        out,
        event::PopKeyboardEnhancementFlags,
        event::DisableBracketedPaste,
        cursor::Show,
        terminal::EndSynchronizedUpdate
    );
    let _ = out.flush();
}

/// Set the tab title from the session name or project basename, stripping controls.
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
