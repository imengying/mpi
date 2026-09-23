//! The terminal view: a scrolling transcript with a pinned two-line footer.
//!
//! The transcript is kept in memory as *unstyled* lines and re-rendered on every change,
//! so collapse/expand and terminal resizes simply rebuild the visible tail. Only the rows
//! pi owns are redrawn — the cursor moves up over them and the rest of the scrollback is
//! left alone, which is what keeps "the chat scrolls up" behaviour working.
//!
//! Input is read in raw mode because Ctrl+O (expand/collapse) never reaches the process in
//! canonical mode. What the line editor offers is deliberately small: editing, history and
//! the few control keys pi defines.

// The rendered-line model and the line editor live in their own modules; re-exported here
// because every consumer already says `ui::screen::Line` and `ui::screen::Editor`, and the
// screen is what they mean by "the terminal part of the UI".
pub use crate::ui::editor::Editor;
pub use crate::ui::terminal::{RawGuard, clear_title, teardown, window_title};
use crate::ui::editor::{common_prefix, input_caret, input_layout};
pub use crate::ui::text::{Bg, Block, Collapsible, Line, Span, Style, wrap_all, wrap_line};

use std::io::{IsTerminal, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{cursor, queue, terminal};

use crate::config::Defaults;
use crate::image_input::{self, PastedImage};
use crate::ui::theme::{Color, Theme};
use crate::util;

/// The name the window title leads with, as pi uses `π`.
///
/// The terminal title is the one piece of chrome the terminal draws itself, so it stays
/// visible when a long output has scrolled the footer away.
pub const APP_TITLE: &str = "π";

/// What the spinner says while a turn is in flight. pi's word, kept as-is: it is the
/// label the user already recognises, and it is not translated.
pub const WORKING_LABEL: &str = "Working";

/// The spinner frames, and how long each is shown.
///
/// Braille, so every frame is exactly one cell: a set mixing widths would make the label to
/// its right jitter. The interval is pi's.
const WORKING_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const WORKING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

/// How many submitted lines the input history keeps.
///
/// A cap rather than a growing list: the arrows are for the message just before this one,
/// and a session that has been running for days should not carry every line it ever saw.
/// Oldest entries fall off the front, so what is kept is what is still plausible.
const HISTORY_LIMIT: usize = 200;


/// What the user did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Line(String),
    /// A line with one or more pasted images attached. The caller sends them as a single
    /// user message so the text and the pictures stay together.
    LineWithImages(String, Vec<PastedImage>),
    ToggleExpand,
    /// Ctrl+C on an empty line, or Ctrl+D.
    Interrupt,
    /// Esc while a turn is in flight: stop it.
    ///
    /// The key is the same one that closes the command menu, and the two do not conflict:
    /// with the menu open it is dismissed first, because that is what an open list means
    /// Esc to do — otherwise closing the menu would throw away the answer being written.
    /// Only at the prompt, where nothing is running, does it stay a no-op.
    Stop,
    Eof,
}

/// A line the user submitted while a turn was running.
///
/// It cannot be acted on where it was typed: a turn is in flight, and starting a second one
/// (or running `/new`, or a picker) underneath it would interleave two conversations. So it
/// waits for the turn it interrupted to finish.
///
/// It waits as *what it is*, not as text. Both kinds look the same in the input line, but
/// they part ways the moment the turn ends: a command has to be dispatched as a command, and
/// a message has to carry the images that were submitted with it. Held as one kind of thing,
/// the two would swap roles on the way out — a queued `/model` handed to the model as prose,
/// and a queued screenshot silently dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    /// A message: sent as the next turn, with whatever images came with it.
    Message(String, Vec<PastedImage>),
    /// A slash command: run once the turn in flight is over.
    Command(String),
}

impl Queued {
    /// The line as the user typed it, for the row that shows what is waiting.
    pub fn text(&self) -> &str {
        match self {
            Queued::Message(text, _) | Queued::Command(text) => text,
        }
    }
}

pub struct Screen {
    out: std::io::Stdout,
    pub theme: Theme,
    width: usize,
    height: usize,
    blocks: Vec<Block>,
    /// How many blocks have already been written to the terminal. Blocks are kept — so an
    /// expand/collapse can re-render one — but each is printed exactly once.
    printed: usize,
    footer: Vec<Line>,
    /// Rows the *live* region (streaming preview plus footer) currently occupies. Only
    /// these are ever redrawn: committed transcript stays in the terminal's scrollback,
    /// which is what makes the history scrollable with the terminal's own keys.
    live_rows: usize,
    /// The line editor's current text, echoed in the live region.
    editing: Option<Editor>,
    history: Vec<String>,
    /// Where the walk through the history currently is: `None` means "not in the history",
    /// so the buffer is the user's own text.
    history_index: Option<usize>,
    /// The text that was in the buffer when the walk into the history began.
    ///
    /// Kept so that walking back down past the newest entry — which is what Down on the last
    /// entry does — restores what was being typed instead of an empty line. Without it, a
    /// half-written message is destroyed by a glance at the history, and the user has no way
    /// to tell that it happened.
    history_draft: Option<Editor>,
    /// Streaming preview: thinking tail plus the answer so far.
    streaming_thinking: Option<String>,
    streaming_answer: Option<String>,
    /// The spinner's label while a turn is in flight, and which frame of the animation it
    /// is on.
    ///
    /// This is pi's indicator: it sits at the top left of the input area and keeps moving
    /// for as long as the turn lasts. A model response and a command can both take minutes
    /// with nothing else to draw, and a spinner that does not move is indistinguishable
    /// from a hung process — which is the one thing the user needs to be able to tell.
    working: Option<String>,
    working_frame: usize,
    /// The tool call in flight, as one line: `● $ sleep 30`.
    ///
    /// A tool can take minutes, and without it the screen would sit unchanged with no sign
    /// that anything is happening. It is a *live* row, not a transcript block: the finished
    /// call is committed afterwards with its output and duration, and this line disappears
    /// in the same redraw.
    running_call: Option<Vec<Span>>,
    /// Whether stdout is a terminal at all.
    interactive: bool,
    /// Whether the terminal cursor is currently shown, so an unchanged state is not written
    /// again on every frame.
    cursor_shown: Option<bool>,
    /// What was last written as the window title, so an unchanged title is not rewritten on
    /// every frame.
    title: Option<String>,
    /// Where the cursor was left inside the live region, as a row index, or `None` when it
    /// was left just past the end. Needed to erase the region from the right place.
    cursor_row: Option<usize>,
    /// The column the cursor was parked on, so a spinner tick can put it back without
    /// redrawing the rows under the spinner.
    cursor_col: usize,
    /// The live lines last painted. A spinner tick compares against this and, when nothing
    /// else moved, rewrites only the spinner row. Clearing the whole region on every frame
    /// is what made the input line and the footer flicker.
    last_live: Vec<Line>,
    /// A one-off line shown under the input, e.g. a failed paste. Cleared on the next edit.
    notice: Option<String>,
    /// Lines submitted while a turn was running, shown above the input until they are sent.
    /// They are *live* rows rather than transcript rows on purpose: the line has not been
    /// acted on yet, so writing it into the transcript would put it above the answer that is
    /// still being written and get the order wrong.
    pending: Vec<Queued>,
    /// Images pasted during the current edit, waiting to be sent with the line.
    ///
    /// Kept out of `editing` because a buffer is text; the image is attached to the message
    /// the line becomes.
    pending_images: Vec<PastedImage>,
    /// Every slash command, for completion and the menu.
    commands: Vec<(String, String)>,
    /// The slash-command menu shown under the input line, and which entry is highlighted.
    menu: Vec<(String, String)>,
    menu_selected: usize,
    /// The buffer Esc dismissed the menu for, if any.
    ///
    /// The menu is rebuilt from the buffer on every keystroke, so without recording the
    /// dismissal there is no way to ask for it to go away: `Esc` was undone by the redraw
    /// that every keypress ends with. It is the *text* rather than a flag so that typing on
    /// brings the menu back — the dismissal is about the command name on screen at that
    /// moment, not about the rest of the line.
    menu_dismissed: Option<String>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    /// Register the commands the input area completes and the menu lists.
    ///
    /// Passed in rather than imported so the screen stays independent of the agent layer,
    /// which is also what keeps it testable without a provider.
    pub fn set_commands(&mut self, commands: &[(&str, &str)]) {
        self.commands = commands
            .iter()
            .map(|(name, help)| ((*name).to_string(), (*help).to_string()))
            .collect();
    }

    /// Fill the input history from a conversation that was already under way.
    ///
    /// The history is the line editor's own, so a screen that has just been built for a
    /// resumed session starts empty — Up would then only ever reach what was typed in *this*
    /// process, and the turns before the resume would look like they had never been typed.
    /// What the user wrote is part of the session, so the arrows have to reach back into it.
    ///
    /// Entries are oldest first, matching the order Up walks them. The list is trimmed from
    /// the front to the same cap `remember` enforces: the newest entries are the ones a
    /// recalled line is likely to be.
    pub fn seed_history(&mut self, lines: impl IntoIterator<Item = String>) {
        self.history = lines.into_iter().filter(|line| !line.trim().is_empty()).collect();
        if self.history.len() > HISTORY_LIMIT {
            let excess = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..excess);
        }
        self.history_index = None;
        self.history_draft = None;
    }

    pub fn new() -> Self {
        let theme = Theme::default();
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let mut screen = Screen {
            out: std::io::stdout(),
            theme,
            width: 80,
            height: 24,
            blocks: Vec::new(),
            printed: 0,
            footer: Vec::new(),
            live_rows: 0,
            editing: None,
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            streaming_thinking: None,
            working: None,
            working_frame: 0,
            running_call: None,
            streaming_answer: None,
            interactive,
            cursor_shown: None,
            title: None,
            cursor_row: None,
            cursor_col: 0,
            last_live: Vec::new(),
            notice: None,
            pending: Vec::new(),
            pending_images: Vec::new(),
            commands: Vec::new(),
            menu: Vec::new(),
            menu_selected: 0,
            menu_dismissed: None,
        };
        screen.refresh_size();
        screen
    }

    pub fn interactive(&self) -> bool {
        self.interactive
    }

    /// Set the terminal's window title to `π - <name> - <directory>`.
    ///
    /// Written with the OSC 0 sequence, the same one pi sends. Nothing is printed when the
    /// output is not a terminal, and an unchanged title is skipped so a redraw does not
    /// spam the terminal with escape sequences.
    pub fn set_title(&mut self, name: Option<&str>, cwd: &Path) {
        if !self.interactive {
            return;
        }
        let title = window_title(name, cwd);
        if self.title.as_deref() == Some(title.as_str()) {
            return;
        }
        // OSC 0;title BEL.
        let _ = write!(self.out, "\u{1b}]0;{title}\u{7}");
        let _ = self.out.flush();
        self.title = Some(title);
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn refresh_size(&mut self) {
        if let Ok((cols, rows)) = terminal::size() {
            self.width = cols.max(20) as usize;
            self.height = rows.max(6) as usize;
        }
    }

    // -- transcript ---------------------------------------------------------

    /// Queue a block. It is written to the terminal on the next redraw — which happens at
    /// the end of the current step, so a note appears immediately but never interleaves
    /// with a half-drawn frame.
    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    pub fn push_lines(&mut self, lines: Vec<Line>) {
        self.blocks.push(Block::lines(lines));
    }

    /// Commit everything queued and redraw the live region.
    pub fn flush(&mut self) {
        self.draw_live();
    }

    /// New work starts collapsed, as pi-custom does.
    pub fn collapse_all(&mut self) {
        for block in &mut self.blocks {
            if let Block::Collapsible(collapsible) = block {
                collapsible.expanded = false;
            }
        }
    }

    /// Flip the most recent collapsible block.
    ///
    /// Output that has already scrolled into the terminal's scrollback cannot be rewritten
    /// in place, so the block is printed again below, in its new state. Re-printing rather
    /// than repainting keeps the history honest: what the user read stays where they read
    /// it, and the fresh copy is the one that is now current.
    ///
    /// Nothing to flip is still an answer: the note is pushed here rather than at each call
    /// site, so a caller cannot forget it and leave the key looking broken.
    pub fn toggle_last_collapsible(&mut self) -> bool {
        let Some(index) = self.blocks.iter().rposition(Block::is_collapsible) else {
            self.push_lines(crate::ui::compact::note_lines(
                "没有可展开的内容",
                Style::new(Color::Dim),
            ));
            self.draw_live();
            return false;
        };
        if let Block::Collapsible(collapsible) = &mut self.blocks[index] {
            collapsible.expanded = !collapsible.expanded;
        }
        let lines = self.blocks[index].render(self.width);
        self.write_lines(&lines, true);
        self.draw_live();
        true
    }

    /// Take the live region down for good, on the way out of the program.
    ///
    /// The region is not part of the transcript: it is the prompt and the footer, redrawn on
    /// every keystroke. Leaving it on screen makes the shell inherit a terminal whose cursor
    /// sits in the middle of a row, and zsh — which marks a partial line with `PROMPT_SP` —
    /// then prints a `%` right after it, so the pi prompt appears to survive as `› /%`.
    ///
    /// Erasing it means the cursor ends up back where the region started, on a line of its
    /// own, which is where the shell expects to find it.
    pub fn leave(&mut self) {
        // Commit first: anything queued since the last draw is part of the transcript — the
        // note naming the command that resumes this session, for one — and erasing without
        // committing would silently throw it away. The note is written *after* the turn loop
        // ends, which is why this cannot be left to a later draw that never comes.
        self.commit();
        self.erase_live();
        if self.interactive {
            let _ = self.out.flush();
        }
    }

    /// Forget the transcript and wipe the screen. Used by `/new` and `/resume`, where the
    /// previous conversation is no longer relevant.
    pub fn clear_transcript(&mut self) {
        self.blocks.clear();
        self.printed = 0;
        self.streaming_answer = None;
        self.streaming_thinking = None;
        self.running_call = None;
        self.working = None;
        self.editing = None;
        self.erase_live();
        if self.interactive {
            let _ = crossterm::execute!(self.out, terminal::Clear(terminal::ClearType::All));
            let _ = crossterm::execute!(self.out, cursor::MoveTo(0, 0));
            let _ = self.out.flush();
        }
    }

    // -- streaming ----------------------------------------------------------

    /// Take the live region down so someone else can draw on the terminal.
    ///
    /// Used before the authorization panel, which writes straight to stdout and moves the
    /// cursor itself: from that point `live_rows`/`cursor_row` no longer describe the screen,
    /// and a later erase would climb from the wrong row and leave the old region behind.
    /// Caller must [`Screen::render`] afterwards to put the region back.
    pub fn suspend_live(&mut self) {
        self.erase_live();
        let _ = self.out.flush();
    }

    /// Start the working spinner with `label`.
    ///
    /// Called once per turn, not per tool call: the spinner covers the whole time the
    /// model or a command has the upper hand, and only the label's neighbours change in
    /// between.
    pub fn set_working(&mut self, label: &str) {
        self.working = Some(label.to_string());
        self.working_frame = 0;
        self.render();
    }

    /// Stop the spinner. The row disappears with the next redraw.
    pub fn clear_working(&mut self) {
        if self.working.take().is_some() {
            self.render();
        }
    }

    /// Advance the spinner and redraw.
    ///
    /// Driven by timers around whatever is blocking, because that is exactly when nothing
    /// else would draw: a command that prints nothing for a minute still has to look alive.
    pub fn tick_working(&mut self) {
        if self.working.is_none() {
            return;
        }
        self.working_frame = (self.working_frame + 1) % WORKING_FRAMES.len();
        if !self.repaint_spinner() {
            self.render();
        }
    }

    /// Show the tool call that is now running, as a single live line.
    ///
    /// Replaced (or cleared with [`Screen::clear_running`]) when the call finishes; the
    /// finished call is committed to the transcript in its place.
    pub fn set_running(&mut self, spans: Vec<Span>) {
        self.running_call = Some(spans);
        self.render();
    }

    /// Take the running line down. Called before the finished call is committed, so the two
    /// never show at once.
    pub fn clear_running(&mut self) {
        if self.running_call.take().is_some() {
            self.render();
        }
    }

    /// Begin a streaming answer: the thinking preview and the text so far live in the live
    /// region at the bottom of the screen until the turn ends.
    pub fn begin_stream(&mut self) {
        self.streaming_thinking = None;
        self.streaming_answer = Some(String::new());
    }

    /// Append to the thinking preview. No redraw: the caller drains a burst of deltas and
    /// redraws once (see `drain_deltas` in the agent loop), and a redraw per token would
    /// erase and repaint the input line — caret included — for every character.
    pub fn push_thinking(&mut self, text: &str) {
        let buffer = self.streaming_thinking.get_or_insert_with(String::new);
        buffer.push_str(text);
    }

    /// Append to the answer preview. No redraw, for the same reason as [`Screen::push_thinking`].
    pub fn push_text(&mut self, text: &str) {
        let buffer = self.streaming_answer.get_or_insert_with(String::new);
        buffer.push_str(text);
    }

    /// Throw away an in-flight preview without committing anything. Used when a request
    /// failed: a half-written answer that was never recorded must not stay on screen.
    pub fn discard_stream(&mut self) {
        self.streaming_answer = None;
        self.streaming_thinking = None;
        self.erase_live();
        let _ = self.out.flush();
    }

    /// Finish the stream and commit the answer. The thinking block itself is not printed:
    /// it collapses to the single "思考完成" marker, matching the compact-display rule.
    pub fn end_stream(&mut self) -> (String, String) {
        let answer = self.streaming_answer.take().unwrap_or_default();
        let thinking = self.streaming_thinking.take().unwrap_or_default();
        let mut lines = Vec::new();
        if !thinking.trim().is_empty() {
            lines.extend(crate::ui::compact::thinking_done_lines());
        }
        if !answer.trim().is_empty() {
            // Rendered at the width that is current *now*, which is the width the answer
            // will keep: committed lines live in the terminal's scrollback, and scrollback
            // cannot be re-flowed.
            lines.extend(crate::ui::markdown::render(&answer, self.width));
            lines.push(Line::blank());
        }
        if !lines.is_empty() {
            self.blocks.push(Block::lines(lines));
        }
        self.flush();
        (answer, thinking)
    }

    // -- history ------------------------------------------------------------

    pub fn set_footer(&mut self, lines: Vec<Line>) {
        self.footer = lines;
    }

    // -- pickers ------------------------------------------------------------

    // -- input --------------------------------------------------------------

    /// Note a line that was submitted while a turn was running.
    ///
    /// The kind is preserved as given: see [`Queued`] for why the two are not both text.
    pub fn queue(&mut self, item: Queued) {
        self.pending.push(item);
        self.render();
    }

    /// Take what was submitted while the turn was running, in order.
    ///
    /// Taking them is what makes them real: the caller acts on them, and until then they
    /// were only rows on screen.
    ///
    /// Images are not touched here. They belong to the *line being written*, not to the
    /// queue: Enter hands them over with the line (see `handle_key`), and a paste that has
    /// not been submitted yet is part of the draft the user is still assembling. Sweeping
    /// them into the queue would send a screenshot the user was still composing, and leave
    /// the message it belonged to without it.
    pub fn take_queued(&mut self) -> Vec<Queued> {
        let queued = std::mem::take(&mut self.pending);
        if !queued.is_empty() {
            self.render();
        }
        queued
    }
}


/// A convenience wrapper for the home directory shortcut.
/// The row that is the working spinner, if this frame has one.
fn spinner_row(lines: &[Line], label: &str) -> Option<usize> {
    lines.iter().position(|line| {
        let spans = &line.spans;
        spans.len() >= 3
            && WORKING_FRAMES.contains(&spans[0].text.as_str())
            && spans[1].text == " "
            && spans[2].text == label
    })
}

/// The first row that has to be repainted, or `None` when a full redraw is needed instead.
///
/// A tick may repaint the rows from here through the spinner — and nothing below it, which is
/// where the input line and its caret live. Everything above the spinner is fair game: the
/// thinking preview is rewritten on every tick by design. The rule is that no row *below* the
/// spinner may differ, because painting those is what erases and redraws the caret.
fn dirty_top(previous: &[Line], next: &[Line], spinner: usize) -> Option<usize> {
    if previous.len() != next.len() || spinner >= next.len() {
        return None;
    }
    // Anything below the spinner moving means the input or footer changed: full redraw.
    if previous[spinner + 1..] != next[spinner + 1..] {
        return None;
    }
    // From the top down, the first row that differs is where repainting starts.
    Some(
        previous[..spinner]
            .iter()
            .zip(&next[..spinner])
            .position(|(old, new)| old != new)
            .unwrap_or(spinner),
    )
}

mod input;
mod live;
mod menu;
mod picker;

#[cfg(test)]
mod tests;
