//! The terminal as a device: raw mode, the window title, and putting both back.
//!
//! Device setup is separate from rendering, so drawing into memory never changes a TTY.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use crossterm::{cursor, event, terminal};

use crate::util;

/// The name the window title leads with, as pi uses `π`.
pub const APP_TITLE: &str = "π";

static EXIT_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Shared with synchronous terminal readers so a signal can end an idle prompt or picker.
pub fn exit_signal() -> Option<i32> {
    let signal = EXIT_SIGNAL.load(Ordering::Relaxed);
    (signal != 0).then_some(signal)
}

/// Receive signals on a runtime worker; cleanup stays on the normal application path.
#[cfg(unix)]
pub fn watch_exit_signals() -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    Ok(tokio::spawn(async move {
        let kind = tokio::select! {
            _ = interrupt.recv() => SignalKind::interrupt(),
            _ = terminate.recv() => SignalKind::terminate(),
            _ = hangup.recv() => SignalKind::hangup(),
        };
        EXIT_SIGNAL.store(kind.as_raw_value(), Ordering::Relaxed);
    }))
}

/// Raw mode belongs to the interactive session and ends at teardown.
/// Nested pickers and approval panels reuse it without changing its lifetime.
pub(crate) fn ensure_raw_mode() -> std::io::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "交互终端不可用",
        ));
    }
    if terminal::is_raw_mode_enabled()? {
        return Ok(());
    }
    terminal::enable_raw_mode()?;
    let result = crossterm::execute!(
        std::io::stdout(),
        terminal::EnableLineWrap,
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
        event::DisableMouseCapture,
        cursor::Show,
        terminal::EndSynchronizedUpdate,
        terminal::LeaveAlternateScreen
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
