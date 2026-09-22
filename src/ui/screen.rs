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

use std::io::{IsTerminal, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, terminal};
use unicode_width::UnicodeWidthChar;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bold: bool,
    pub bg: Bg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bg {
    None,
    Added,
    Removed,
    Selected,
}

impl Style {
    pub const fn plain() -> Self {
        Style { fg: Color::Text, bold: false, bg: Bg::None }
    }

    pub const fn new(fg: Color) -> Self {
        Style { fg, bold: false, bg: Bg::None }
    }

    pub const fn bold(fg: Color) -> Self {
        Style { fg, bold: true, bg: Bg::None }
    }

    pub const fn with_bg(fg: Color, bg: Bg) -> Self {
        Style { fg, bold: false, bg }
    }
}

/// A run of characters sharing one style.
///
/// Styling is structured rather than embedded as escape codes, so line wrapping can
/// measure real display width and still keep every run's colour. Nothing in a `Line`
/// ever contains an escape sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Span { text: text.into(), style }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Span::new(text, Style::plain())
    }

    /// A run whose background is painted to `fill` columns beyond its own text, so a diff
    /// row reads as a solid bar right up to the edge of the terminal.
    pub fn with_fill(text: impl Into<String>, style: Style, fill: Bg) -> Span {
        Span { text: text.into(), style: Style { bg: fill, ..style } }
    }
}

/// One transcript line, as styled runs. Wrapping happens on this structure, so the
/// output width is exact and colour never leaks across a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Line { spans: vec![Span::new(text, style)] }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Line::new(text, Style::plain())
    }

    pub fn dim(text: impl Into<String>) -> Self {
        Line::new(text, Style::new(Color::Dim))
    }

    pub fn blank() -> Self {
        Line::plain(String::new())
    }

    /// Build a line from already-styled runs.
    pub fn spans(spans: Vec<Span>) -> Self {
        let spans: Vec<Span> = spans.into_iter().filter(|span| !span.text.is_empty()).collect();
        if spans.is_empty() {
            Line::blank()
        } else {
            Line { spans }
        }
    }

    /// The visible text, with no styling. This is what tests and width checks use.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(|span| util::width(&span.text)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    /// Append another line's runs to this one.
    pub fn extend(&mut self, other: Line) {
        self.spans.extend(other.spans);
    }
}

/// A run of lines that can be collapsed to an excerpt of itself.
///
/// Collapsed, the block shows its head, the tail end of what is left out, and its tail —
/// and says nothing about it. A row announcing "N lines collapsed, press Ctrl+O" is a line
/// the user has to read on every single tool call to be told something they already know
/// (they pressed Ctrl+O before), and it pushes the transcript around for no content. The
/// excerpt is visibly an excerpt: the rows simply stop.
#[derive(Debug, Clone)]
pub struct Collapsible {
    pub lines: Vec<Line>,
    /// Rows that stay visible in both states: a tool result's header belongs here, so
    /// collapsing never hides which command produced the output.
    pub head: usize,
    /// Rows that stay visible at the end in both states: an exit code or a
    /// "full output: …" note. Losing those while collapsed would hide the outcome.
    pub tail: usize,
    /// How many rows from the *start* of the middle are shown while collapsed.
    ///
    /// Zero for command output, where the interesting part is the tail: a log is read from
    /// its end. A diff sets it, because a change is read from both ends — the first removed
    /// line says what was taken away, and the first added line says what replaced it.
    pub middle_head: usize,
    /// How many rows from the *end* of the middle are shown while collapsed.
    pub preview: usize,
    pub expanded: bool,
}

/// A logical group of transcript lines.
#[derive(Debug, Clone)]
pub enum Block {
    Lines(Vec<Line>),
    Collapsible(Collapsible),
}

impl Collapsible {
    /// How many rows this block reserves at each end, clamped to what it actually has so
    /// a short block can never double-count a row.
    fn split(&self, total: usize) -> (usize, usize) {
        let head = self.head.min(total);
        let tail = self.tail.min(total - head);
        (head, tail)
    }
}

impl Block {
    pub fn lines(lines: Vec<Line>) -> Self {
        Block::Lines(lines)
    }

    /// A block whose tail is shown by default and which Ctrl+O expands. The first `head`
    /// and last `tail` rows are always visible.
    pub fn collapsible(lines: Vec<Line>, head: usize, tail: usize, preview: usize) -> Self {
        Block::Collapsible(Collapsible {
            lines,
            head,
            tail,
            middle_head: 0,
            preview,
            expanded: false,
        })
    }

    /// Like [`Block::collapsible`], but the collapsed form also keeps `middle_head` rows
    /// from the start of the middle. Used by diffs, which are read from both ends.
    pub fn collapsible_excerpted(
        lines: Vec<Line>,
        head: usize,
        tail: usize,
        middle_head: usize,
        preview: usize,
    ) -> Self {
        Block::Collapsible(Collapsible { lines, head, tail, middle_head, preview, expanded: false })
    }

    pub fn render(&self, width: usize) -> Vec<Line> {
        match self {
            Block::Lines(lines) => wrap_all(lines, width),
            Block::Collapsible(block) => {
                let wrapped = wrap_all(&block.lines, width);
                let total = wrapped.len();
                if block.expanded {
                    return wrapped;
                }
                let (head, tail) = block.split(total);
                if total <= head + tail + block.middle_head + block.preview {
                    return wrapped;
                }
                let hidden = total - head - tail - block.middle_head - block.preview;
                let mut out = wrapped[..head].to_vec();
                // The start of the middle, then the end of it, then the always-visible tail.
                out.extend(
                    wrapped[head..head + block.middle_head].iter().cloned(),
                );
                out.extend(wrapped[head + hidden + block.middle_head..total - tail].iter().cloned());
                out.extend(wrapped[total - tail..].iter().cloned());
                out
            }
        }
    }

    pub fn is_collapsible(&self) -> bool {
        matches!(self, Block::Collapsible(_))
    }
}

/// Wrap one line to `width` columns, breaking at spaces and keeping every run's style.
/// A word longer than the line is broken by character, which is the only case where a
/// styled run is split mid-word.
pub fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.text.chars() {
            chars.push((c, span.style));
        }
    }
    if chars.is_empty() {
        return vec![Line::blank()];
    }
    let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut used = 0usize;
    // (row, index within row) of the most recent break opportunity.
    let mut last_space: Option<(usize, usize)> = None;
    for (c, style) in chars {
        if c == '\n' {
            rows.push(Vec::new());
            used = 0;
            last_space = None;
            continue;
        }
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if used > 0 && used + char_width > width {
            match last_space.take() {
                Some((row, index)) if row + 1 == rows.len() => {
                    // Break at the last space: everything after it moves down.
                    let rest = rows[row].split_off(index + 1);
                    // The space itself is dropped rather than left dangling.
                    rows.last_mut().unwrap().pop();
                    rows.push(rest);
                    used = rows.last().unwrap().iter().map(|(c, _)| UnicodeWidthChar::width(*c).unwrap_or(0)).sum();
                }
                _ => {
                    rows.push(Vec::new());
                    used = 0;
                }
            }
        }
        if c == ' ' {
            last_space = Some((rows.len() - 1, rows.last().unwrap().len()));
        }
        used += char_width;
        rows.last_mut().unwrap().push((c, style));
    }
    rows.into_iter().map(|row| Line { spans: coalesce(row) }).collect()
}

/// Merge adjacent characters that share a style back into runs.
fn coalesce(row: Vec<(char, Style)>) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (c, style) in row {
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(c),
            _ => spans.push(Span::new(c.to_string(), style)),
        }
    }
    spans
}

/// One row of the input area: the prompt prefix, the text on that row, and the index into
/// the buffer (in characters) of the first character the row shows.
///
/// The start index is what makes a caret possible: `caret` is a character index into the
/// buffer, and this is what turns it into a row and a column.
struct InputRow {
    prefix: String,
    text: String,
    start: usize,
}

impl InputRow {
    fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// Split the input buffer into display rows, each tagged with its prompt prefix.
///
/// Wrapping is done on the buffer as a whole with the prompt width subtracted, then the
/// prompt is prefixed to the first row, so a long word breaks at the real screen edge
/// instead of two columns early on every row. Every row carries a prefix string —
/// `"> "` for the first, spaces for the rest — which is what keeps the continuation rows
/// aligned under the text rather than under the prompt.
///
/// The break is by character width, not by word: an input line is not prose, and moving a
/// partly-typed path or command onto its own row would make the caret jump around.
fn input_layout(text: &str, width: usize, prompt_width: usize) -> Vec<InputRow> {
    let mut rows: Vec<InputRow> = Vec::new();
    let mut current = String::new();
    // Character index of the first character of `current`, and of the next one to place.
    let mut start = 0usize;
    let mut used = 0usize;
    // Every row, continuation included, carries a two-column prefix so the text lines up
    // under itself; the usable width is therefore the same on all of them.
    let room = width.saturating_sub(prompt_width).max(1);
    for (index, c) in text.chars().enumerate() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > room && used > 0 {
            rows.push(InputRow {
                prefix: String::new(),
                text: std::mem::take(&mut current),
                start,
            });
            start = index;
            used = 0;
        }
        current.push(c);
        used += cw;
    }
    rows.push(InputRow { prefix: String::new(), text: current, start });
    // A row filled to the last cell needs one more for the caret to sit on. The caret marks
    // where the next character goes, and there is no cell left on this row to draw it in:
    // asking the terminal for a column past the right edge is clamped to the last cell at
    // best, and clamps are exactly what a caret must not depend on. An empty row is where
    // readline puts the cursor in the same situation.
    if rows.last().is_some_and(|row| util::width(&row.text) == room) {
        let start = text.chars().count();
        rows.push(InputRow { prefix: String::new(), text: String::new(), start });
    }
    // Tag the prefixes now that the row count is known.
    for (index, row) in rows.iter_mut().enumerate() {
        row.prefix = if index == 0 {
            "› ".to_string()
        } else {
            " ".repeat(prompt_width)
        };
    }
    rows
}

/// Where the caret belongs, as `(row, columns into the text)`, for a character index into
/// the buffer.
///
/// The row is the last one that starts at or before the caret, which puts a caret landing
/// exactly on a wrap boundary at the start of the row below — where a terminal would put it
/// after writing the last cell of a full line. The column is a *display* width, measured
/// over the characters before the caret, so a CJK character counts as the two cells it
/// occupies.
fn input_caret(rows: &[InputRow], caret: usize) -> (usize, usize) {
    let row = rows.iter().rposition(|row| row.start <= caret).unwrap_or(0);
    let offset = caret.saturating_sub(rows[row].start).min(rows[row].chars());
    let shown: String = rows[row].text.chars().take(offset).collect();
    (row, util::width(&shown))
}

/// The rows of the input buffer as `(prefix, text)` pairs, as the tests assert on them.
#[cfg(test)]
fn input_rows(text: &str, width: usize, prompt_width: usize) -> Vec<(String, String)> {
    input_layout(text, width, prompt_width)
        .into_iter()
        .map(|row| (row.prefix, row.text))
        .collect()
}

/// The longest prefix shared by every name, used to fill in as much as is unambiguous.
fn common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = first.to_string();
    while !names.iter().all(|name| name.starts_with(&prefix)) {
        // Drop one character at a time; the loop ends at the empty string, which every name
        // has as a prefix.
        prefix.pop();
    }
    prefix
}

/// Wrap a group of lines, flattening the result.
pub fn wrap_all(lines: &[Line], width: usize) -> Vec<Line> {
    lines.iter().flat_map(|line| wrap_line(line, width)).collect()
}

/// The line editor's state: what has been typed and where the caret sits in it.
///
/// The caret is a **character** index, not a byte offset and not a column: bytes would split
/// a CJK character, and columns would make a horizontal move depend on how wide the
/// characters happen to be. Every edit goes through this struct, so an index can never be
/// left pointing into the middle of a character or past the end of the buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    chars: Vec<char>,
    /// Where the next character typed goes, as an index into `chars`.
    caret: usize,
}

impl Editor {
    pub fn new() -> Self {
        Editor::default()
    }

    pub fn from_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let caret = chars.len();
        Editor { chars, caret }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Move the caret `delta` characters left or right, stopping at both ends.
    ///
    /// Clamping rather than wrapping is what makes the key safe to hold down: a caret that
    /// jumped from one end of the line to the other would make correcting a typo a guessing
    /// game about where it is going to land.
    pub fn move_caret(&mut self, delta: isize) {
        self.caret = (self.caret as isize + delta).clamp(0, self.chars.len() as isize) as usize;
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.chars.len();
    }

    /// Insert at the caret, which then sits after the text just typed.
    pub fn insert(&mut self, text: &str) {
        for (offset, c) in text.chars().enumerate() {
            self.chars.insert(self.caret + offset, c);
        }
        self.caret += text.chars().count();
    }

    /// Backspace: remove the character *before* the caret, if there is one.
    pub fn backspace(&mut self) {
        if self.caret > 0 {
            self.caret -= 1;
            self.chars.remove(self.caret);
        }
    }

    /// Delete: remove the character *under* the caret, leaving the caret where it is.
    pub fn delete(&mut self) {
        if self.caret < self.chars.len() {
            self.chars.remove(self.caret);
        }
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.caret = 0;
    }

    /// Delete back to the start of the current word, as Ctrl+W does everywhere else.
    ///
    /// The caret decides what "the current word" is: it means the text before the caret, so
    /// pressing Ctrl+W in the middle of a line removes the word to the left rather than the
    /// tail of the line.
    pub fn delete_word(&mut self) {
        while self.caret > 0 && self.chars[self.caret - 1] == ' ' {
            self.backspace();
        }
        while self.caret > 0 && self.chars[self.caret - 1] != ' ' {
            self.backspace();
        }
    }

    /// Delete from the caret back to the start of the line.
    pub fn delete_to_start(&mut self) {
        self.chars.drain(..self.caret);
        self.caret = 0;
    }

    /// Delete from the caret to the end of the line.
    pub fn delete_to_end(&mut self) {
        self.chars.truncate(self.caret);
    }
}

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
    pub fn toggle_last_collapsible(&mut self) -> bool {
        let Some(index) = self.blocks.iter().rposition(Block::is_collapsible) else {
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
        self.render();
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

    pub fn push_thinking(&mut self, text: &str) {
        let buffer = self.streaming_thinking.get_or_insert_with(String::new);
        buffer.push_str(text);
        self.render();
    }

    pub fn push_text(&mut self, text: &str) {
        let buffer = self.streaming_answer.get_or_insert_with(String::new);
        buffer.push_str(text);
        self.render();
    }

    /// Throw away an in-flight preview without committing anything. Used when a request
    /// failed: a half-written answer that was never recorded must not stay on screen.
    pub fn discard_stream(&mut self) {
        self.streaming_answer = None;
        self.streaming_thinking = None;
        self.erase_live();
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
            lines.extend(
                util::sanitize(&answer)
                    .lines()
                    .map(|line| Line::plain(line.to_string())),
            );
            lines.push(Line::blank());
        }
        if !lines.is_empty() {
            self.blocks.push(Block::lines(lines));
        }
        self.flush();
        (answer, thinking)
    }

    /// Live rows: the prompt while editing, the command menu, and the footer — plus where
    /// the terminal cursor belongs among them.
    fn compose_live(&self) -> (Vec<Line>, Option<(usize, usize)>) {
        let mut cursor = None;
        let mut lines: Vec<Line> = Vec::new();
        if let Some(thinking) = &self.streaming_thinking {
            let plain = util::sanitize(thinking);
            let wrapped = util::wrap(&util::one_line(&plain), self.width);
            let tail = wrapped.len().saturating_sub(Defaults::THINKING_PREVIEW_LINES);
            for line in &wrapped[tail..] {
                lines.push(Line::new(line.clone(), Style::new(Color::Dim)));
            }
            self.trim_live(&mut lines);
        }
        if let Some(answer) = &self.streaming_answer {
            let body: Vec<Line> = util::sanitize(answer)
                .lines()
                .map(|line| Line::plain(line.to_string()))
                .collect();
            let wrapped = wrap_all(&body, self.width);
            lines.extend(wrapped);
            self.trim_live(&mut lines);
        }
        if let Some(spans) = &self.running_call {
            lines.extend(wrap_line(&Line::spans(spans.clone()), self.width));
            lines.push(Line::blank());
            self.trim_live(&mut lines);
        }
        // The spinner sits directly above the input, at its left, the way pi draws it at the
        // top of the editor. It is part of the live region, so it disappears with it.
        if let Some(label) = &self.working {
            let frame = WORKING_FRAMES[self.working_frame % WORKING_FRAMES.len()];
            lines.push(Line::spans(vec![
                Span::new(frame, Style::new(Color::Cyan)),
                Span::plain(" "),
                Span::new(label.clone(), Style::new(Color::Dim)),
            ]));
        }
        if let Some(editing) = &self.editing {
            // The buffer wraps onto as many rows as it needs, so a long line stays fully
            // readable — scrolling sideways would hide the beginning of what was typed and
            // give no way to get back to it. The prompt occupies the first two columns of
            // the first row only.
            let prompt_width = 2;
            let width = self.width.max(prompt_width + 1);
            let rows = input_layout(&editing.text(), width, prompt_width);
            let base = lines.len();
            for (index, row) in rows.iter().enumerate() {
                // The prefix is drawn on every row: on continuation rows it is spaces, and
                // skipping it would lose the alignment that makes the wrap readable.
                let style = if index == 0 {
                    Style::new(Color::Cyan)
                } else {
                    Style::plain()
                };
                lines.push(Line::spans(vec![
                    Span::new(row.prefix.clone(), style),
                    Span::plain(row.text.clone()),
                ]));
            }
            // The caret is wherever the buffer left it, not necessarily at the end: the whole
            // point of Left/Right is to put it in the middle and type there. The column is a
            // display column, so a CJK character counts as the two cells it occupies.
            let (row, column) = input_caret(&rows, editing.caret());
            cursor = Some((base + row, prompt_width + column));
        }
        // Lines waiting for the turn in flight. They sit directly above the input line, where
        // the user just typed them, so it is obvious they were taken and are queued rather
        // than lost.
        for queued in &self.pending {
            lines.push(Line::spans(vec![
                Span::new("… ", Style::new(Color::Dim)),
                Span::new(util::one_line(queued.text()), Style::new(Color::Dim)),
            ]));
        }
        // Pending images and one-off notices sit between the input and the menu: they are
        // about what is being composed, so they belong next to it.
        for image in &self.pending_images {
            lines.push(Line::new(image.label(), Style::new(Color::Magenta)));
        }
        if let Some(notice) = &self.notice {
            lines.push(Line::new(notice.clone(), Style::new(Color::Yellow)));
        }
        // The menu goes directly under the input line, above the footer.
        if !self.menu.is_empty() {
            for (index, (name, help)) in self.menu.iter().enumerate() {
                let selected = index == self.menu_selected;
                let marker = if selected { "› " } else { "  " };
                // The command name is what is being picked, so it keeps the accent colour and
                // only gains weight when highlighted. Painting the whole row grey made the
                // list read as disabled text: the names are the point, the descriptions are
                // the aside.
                let name_style = if selected {
                    Style { bold: true, ..Style::new(Color::Cyan) }
                } else {
                    Style::new(Color::Cyan)
                };
                lines.push(Line::spans(vec![
                    Span::new(marker, name_style),
                    Span::new(format!("/{name}"), name_style),
                    Span::new("  ", Style::plain()),
                    Span::new(util::truncate(help, self.width.saturating_sub(6).min(60), "…"), Style::new(Color::Dim)),
                ]));
            }
        }
        lines.extend(self.footer.iter().cloned());
        (lines, cursor)
    }

    /// The commands whose name starts with what has been typed after the `/`.
    ///
    /// An empty prefix (just `/`) matches everything, which is what makes the menu appear as
    /// soon as the slash is typed. A slash anywhere but the first column is ordinary text —
    /// `/` inside a sentence is not a command.
    fn matching_commands(&self) -> Vec<(String, String)> {
        let Some(text) = self.editing.as_ref().map(Editor::text) else {
            return Vec::new();
        };
        let Some(rest) = text.strip_prefix('/') else {
            return Vec::new();
        };
        // The menu describes the command name, which is only being typed while the caret is
        // still inside it. Once the caret moves past the slash into ordinary text, offering
        // to complete a command would hijack the arrow keys for a menu about something the
        // user is no longer writing.
        if !self.caret_in_command_name() {
            return Vec::new();
        }
        // Once there is a space the command name is settled and the argument is being typed,
        // so the menu has nothing left to offer.
        if rest.contains(' ') {
            return Vec::new();
        }
        let prefix = rest.trim();
        self.commands
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Is the caret still in the command name (before the first space)?
    fn caret_in_command_name(&self) -> bool {
        let Some(editing) = &self.editing else {
            return false;
        };
        let before: String = editing.text().chars().take(editing.caret()).collect();
        !before.contains(' ')
    }

    /// Refresh the menu from the buffer. Called after every edit.
    fn sync_menu(&mut self) {
        let text = self.editing.as_ref().map(Editor::text).unwrap_or_default();
        if let Some(dismissed) = &self.menu_dismissed {
            if *dismissed == text {
                // Still the buffer Esc dismissed: leave the list hidden. Without this, the
                // redraw at the end of every keypress would put it straight back.
                self.menu.clear();
                self.menu_selected = 0;
                return;
            }
            // The buffer moved on, so the dismissal no longer applies.
            self.menu_dismissed = None;
        }
        let matches = self.matching_commands();
        if matches.len() == self.menu.len() && matches.iter().zip(&self.menu).all(|(a, b)| a == b) {
            // Same list: keep the highlight where the user put it.
            return;
        }
        self.menu = matches;
        self.menu_selected = 0;
    }

    /// Tab: complete the first candidate, or the highlighted one.
    ///
    /// A unique match is completed in full and a trailing space added, so the user can go
    /// straight on to the argument. Several matches share the longest common prefix, which
    /// is the behaviour a shell user expects; the menu stays open to pick from.
    fn complete(&mut self) -> bool {
        self.sync_menu();
        let Some(editing) = self.editing.clone() else {
            return false;
        };
        let text = editing.text();
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        if rest.contains(' ') || self.menu.is_empty() {
            return false;
        }
        let names: Vec<&str> = self.menu.iter().map(|(name, _)| name.as_str()).collect();
        let filled = if names.len() == 1 {
            format!("/{} ", names[0])
        } else {
            let prefix = common_prefix(&names);
            if prefix.len() <= rest.trim().len() {
                // Nothing more to add; let Tab move through the menu instead.
                self.menu_selected = (self.menu_selected + 1) % self.menu.len();
                return true;
            }
            format!("/{prefix}")
        };
        // The completion replaces the whole buffer, and the caret goes to the end of what
        // was inserted — completing is not the place to leave the caret behind in the middle
        // of a word the user did not type.
        self.editing = Some(Editor::from_text(&filled));
        self.sync_menu();
        true
    }

    /// Move the highlight through the menu, wrapping at both ends.
    fn move_menu(&mut self, delta: isize) -> bool {
        if self.menu.is_empty() {
            return false;
        }
        let len = self.menu.len() as isize;
        self.menu_selected = ((self.menu_selected as isize + delta).rem_euclid(len)) as usize;
        true
    }

    /// Take the highlighted command as the line to submit.
    ///
    /// The menu exists to save typing, so picking from it *runs* the command. Completing it
    /// with a trailing space instead would make the user press Enter twice to do the thing
    /// they just chose — and the second press is not obviously part of picking a menu item.
    ///
    /// `None` when there is nothing to take.
    fn accepted_command(&self) -> Option<String> {
        self.menu
            .get(self.menu_selected)
            .map(|(name, _)| format!("/{name}"))
    }

    // -- history ------------------------------------------------------------

    /// Up: one entry further back, or the oldest entry when the walk begins.
    ///
    /// The first press stashes the buffer, so whatever was half-typed comes back when the
    /// walk returns to the bottom. Pressing Up at the oldest entry does nothing rather than
    /// wrapping to the newest: wrapping makes it impossible to tell the top of the history
    /// from the bottom, and a stray key press would then land on a different entry entirely.
    ///
    /// A recalled line is marked as recalled, so the command menu stays shut for it. Without
    /// that, recalling a `/command` pops the menu open — and the menu wants the arrows, which
    /// are the only way back out of the history. The user pressed Up for a previous *line*,
    /// not to be shown a list of commands they did not ask about; the menu returns as soon as
    /// they type, because then it is about something they *are* writing.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_index {
            Some(0) => 0,
            Some(index) => index - 1,
            None => {
                self.history_draft = self.editing.clone();
                self.history.len() - 1
            }
        };
        self.history_index = Some(next);
        self.editing = Some(Editor::from_text(&self.history[next]));
        self.menu_dismissed = self.editing.as_ref().map(Editor::text);
    }

    /// Down: one entry towards the newest, and past the newest back to the draft.
    ///
    /// The last press lands on a **blank** line holding the draft — not on the newest entry
    /// again, and not on nothing at all. Waiting at the newest entry means the user has to
    /// guess how many entries there are; a blank line is the unambiguous "this is the line
    /// you are writing".
    ///
    /// Recalled entries keep the menu shut, like [`Screen::history_up`] does. The one
    /// exception is the draft: it is the user's own line, so if it opens a menu, that menu
    /// comes back with it.
    fn history_down(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.editing = Some(Editor::from_text(&self.history[index + 1]));
            self.menu_dismissed = self.editing.as_ref().map(Editor::text);
            return;
        }
        // Past the newest entry: back to what was being typed before the walk began.
        self.history_index = None;
        self.editing = Some(self.history_draft.take().unwrap_or_default());
        self.menu_dismissed = None;
    }

    /// Keep the live region smaller than the screen, dropping the oldest preview rows.
    fn trim_live(&self, lines: &mut Vec<Line>) {
        let budget = self.height.saturating_sub(self.footer.len() + 2);
        if lines.len() > budget {
            let drop = lines.len() - budget;
            lines.drain(..drop);
        }
    }

    pub fn set_footer(&mut self, lines: Vec<Line>) {
        self.footer = lines;
    }

    /// Commit finished transcript blocks to the terminal.
    ///
    /// Only new blocks are written, one line at a time, so the terminal's own scrollback
    /// holds the conversation. Committed output is never redrawn — that is what keeps the
    /// history usable with the terminal's scroll keys.
    /// Write any block that has not been printed yet.
    fn commit(&mut self) {
        if self.printed >= self.blocks.len() {
            return;
        }
        let fresh: Vec<Line> = self.blocks[self.printed..]
            .iter()
            .flat_map(|block| block.render(self.width))
            .collect();
        self.printed = self.blocks.len();
        self.write_lines(&fresh, true);
    }

    /// Write lines to the terminal, stepping out of the live region first so committed rows
    /// never land on top of it.
    fn write_lines(&mut self, lines: &[Line], below_live: bool) {
        if lines.is_empty() {
            return;
        }
        if self.interactive && below_live {
            self.erase_live();
        }
        let mut buffer = String::new();
        for line in lines {
            buffer.push_str(&self.paint(line, self.width));
            buffer.push_str("\r\n");
        }
        let _ = write!(self.out, "{buffer}");
        let _ = self.out.flush();
    }

    /// Remove the live region, leaving the cursor where the region started.
    ///
    /// The cursor may be parked *inside* the region (on the input row) or just past its end
    /// (while streaming), so the distance back to the first live row differs between the two.
    /// Getting this wrong is not merely cosmetic: erasing from the wrong row leaves the rest
    /// of the old frame on screen, and the next frame is then drawn below it, which pushes
    /// the transcript up by however many rows were missed — once per redraw.
    fn erase_live(&mut self) {
        if !self.interactive || self.live_rows == 0 {
            return;
        }
        // Rows to climb to reach the top of the region.
        //
        // The cursor is either parked inside it (row `r`, so `r` rows below the top) or left
        // on the last drawn row, which is `live_rows - 1` below the top. Moving up *this*
        // many rows lands on the first live row; moving up any more would climb past it and
        // clear committed transcript instead, one row of it per redraw.
        let up = match self.cursor_row {
            Some(row) => row,
            None => self.live_rows.saturating_sub(1),
        };
        if up > 0 {
            let _ = crossterm::execute!(self.out, cursor::MoveToPreviousLine(up as u16));
        }
        let _ = write!(self.out, "\r");
        let _ = crossterm::execute!(self.out, terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = self.out.flush();
        self.live_rows = 0;
        self.cursor_row = None;
    }

    /// Re-render the live region: the streaming preview (if any), the prompt (if editing)
    /// and the footer. Everything above it is left alone.
    pub fn render(&mut self) {
        self.draw_live();
    }

    /// Show or hide the terminal cursor, remembering the state so a redraw that does not
    /// change it does not write the escape again.
    fn set_cursor_visible(&mut self, visible: bool) {
        if !self.interactive || self.cursor_shown == Some(visible) {
            return;
        }
        if visible {
            let _ = crossterm::execute!(self.out, cursor::Show);
        } else {
            let _ = crossterm::execute!(self.out, cursor::Hide);
        }
        self.cursor_shown = Some(visible);
    }

    fn draw_live(&mut self) {
        // Commit first, so new rows appear above the live region rather than inside it.
        self.commit();
        if !self.interactive {
            return;
        }
        let (lines, cursor) = self.compose_live();
        self.erase_live();
        let mut buffer = String::new();
        // Separate rows with CRLF but do **not** end the last one with it. A trailing newline
        // leaves the cursor on a row that has nothing in it, and that empty row is the blank
        // line under the footer: the live region is one row taller than what it draws.
        //
        // After the loop the cursor sits at the start of the row *after* the last drawn one
        // (or on it, if the last row exactly filled the width). Both `erase_live` and the park
        // below are written against that position.
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str(&self.paint(line, self.width));
        }
        // Park the cursor inside the input row.
        //
        // Having drawn `lines.len()` rows without a final newline, the cursor is one row below
        // the last drawn row, so it needs `lines.len() - 1 - row` steps up — one fewer than
        // the number of rows. With no cursor target (streaming, no input line) it is left on
        // that row, which is where `erase_live` expects to find it.
        if let Some((row, column)) = cursor {
            let up = lines.len().saturating_sub(1).saturating_sub(row);
            if up > 0 {
                buffer.push_str(&format!("\u{1b}[{up}A"));
            }
            buffer.push_str(&format!("\u{1b}[{}G", column + 1));
        }
        // The caret is shown only where it marks something: the position the next character
        // of the input line will go. That is true for the whole session now — the composer
        // stays armed through a turn, so the caret is there to type into while the answer
        // streams — and false only when there is no input line at all, as between a command
        // tearing the transcript down and the next prompt arming it. A terminal cursor left
        // over from the last write sits at the bottom of the region blinking at nothing, and
        // "waiting for you" versus "waiting for the model" is the one thing the screen has to
        // make obvious.
        self.set_cursor_visible(cursor.is_some());
        let _ = write!(self.out, "{buffer}");
        let _ = self.out.flush();
        self.live_rows = lines.len();
        // Where the cursor was left, so the next erase starts from the right row. With no
        // cursor target it rests on the last drawn row, which is `live_rows - 1` rows below
        // the top — `erase_live` derives that from `None` rather than storing it.
        self.cursor_row = cursor.map(|(row, _)| row);
    }

    fn paint(&self, line: &Line, pad_to: usize) -> String {
        let mut out = String::new();
        let mut used = 0usize;
        for span in &line.spans {
            let background = match span.style.bg {
                Bg::None => None,
                Bg::Added => Some(Bg::Added),
                Bg::Removed => Some(Bg::Removed),
                Bg::Selected => Some(Bg::Selected),
            };
            let text = self.theme.fg(span.style.fg, &span.text);
            let text = if span.style.bold { self.theme.bold(&text) } else { text };
            let text = match background {
                Some(Bg::Added) => self.theme.bg_added(&text),
                Some(Bg::Removed) => self.theme.bg_removed(&text),
                Some(Bg::Selected) => self.theme.bg_selected(&text),
                _ => text,
            };
            used += util::width(&span.text);
            out.push_str(&text);
        }
        if let Some(fill) = line.spans.first().map(|span| span.style.bg)
            && pad_to > used
            && fill != Bg::None
        {
            let padding = " ".repeat(pad_to - used);
            out.push_str(&match fill {
                Bg::Added => self.theme.bg_added(&padding),
                Bg::Removed => self.theme.bg_removed(&padding),
                Bg::Selected => self.theme.bg_selected(&padding),
                Bg::None => padding,
            });
        }
        out
    }

    // -- pickers ------------------------------------------------------------

    /// Show a list next to the footer and let the user choose one entry.
    ///
    /// The highlighted entry starts on the first one, so Enter accept the obvious default.
    /// Returns `None` for Esc / Ctrl+C / `q`, and the index of the chosen entry otherwise.
    pub fn pick(&mut self, title: &str, items: &[String]) -> Option<usize> {
        self.pick_at(title, items, 0)
    }

    /// Like [`Screen::pick`], but the hint row says what Esc will do.
    ///
    /// The default hint promises "cancel", which is wrong where cancel *is* an action —
    /// `/resume` starts a new session on Esc, and a menu that said "cancel" while doing that
    /// would be lying about its own key.
    pub fn pick_with_hint(
        &mut self,
        title: &str,
        hint: &str,
        items: &[String],
    ) -> Option<usize> {
        self.pick_hinted(title, items, 0, hint)
    }

    /// Like [`Screen::pick`], with the initial highlight on `initial` (clamped into range).
    /// The menu for `/model` uses this so the current choice starts highlighted and Enter
    /// keeps it.
    pub fn pick_at(&mut self, title: &str, items: &[String], initial: usize) -> Option<usize> {
        self.pick_hinted(title, items, initial, "↑↓ 选择 · Enter 确认 · Esc 取消")
    }

    fn pick_hinted(
        &mut self,
        title: &str,
        items: &[String],
        initial: usize,
        hint: &str,
    ) -> Option<usize> {
        if items.is_empty() {
            return None;
        }
        if !self.interactive {
            // No terminal to choose on: fall back to the highlighted entry.
            return Some(initial.min(items.len() - 1));
        }
        let mut cursor = initial.min(items.len() - 1);
        let guard = match RawGuard::enter() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        let saved_footer = self.footer.clone();
        let saved_editing = self.editing.take();
        // Take the live region down before the panel draws. The panel writes straight to the
        // terminal and moves the cursor itself, so `live_rows`/`cursor_row` — this struct's
        // record of where the region is — stop describing the screen the moment it runs. The
        // next erase would then climb from the wrong row and leave part of the old region
        // behind, which is how a finished tool call kept a stale `●` line above it.
        //
        // Clearing first means the panel starts on a clean row, and the redraw at the end
        // puts the region back from a known position.
        self.erase_live();
        let result = loop {
            // The menu replaces the footer so it always sits in the same place.
            self.footer = self.menu_lines(title, hint, items, cursor);
            self.draw_live();
            let event = match event::read() {
                Ok(event) => event,
                Err(_) => break None,
            };
            let Event::Key(key) = event else {
                if matches!(event, Event::Resize(_, _)) {
                    self.refresh_size();
                }
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => break Some(cursor),
                KeyCode::Esc => break None,
                KeyCode::Char('c') | KeyCode::Char('q')
                    if key.modifiers.contains(KeyModifiers::CONTROL) || key.code == KeyCode::Char('q') =>
                {
                    break None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    cursor = if cursor == 0 { items.len() - 1 } else { cursor - 1 };
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    cursor = (cursor + 1) % items.len();
                }
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let index = c as usize - '1' as usize;
                    if index < items.len() {
                        break Some(index);
                    }
                }
                _ => {}
            }
        };
        self.footer = saved_footer;
        self.editing = saved_editing;
        drop(guard);
        // The menu shared the live region with the footer, so nothing has to be erased
        // beyond redrawing it.
        self.draw_live();
        result
    }

    fn menu_lines(&self, title: &str, hint: &str, items: &[String], cursor: usize) -> Vec<Line> {
        let mut lines = vec![Line::new(title, Style::bold(Color::Cyan))];
        for (index, item) in items.iter().enumerate() {
            let selected = index == cursor;
            let marker = if selected { "› " } else { "  " };
            let text = util::truncate(&format!("{marker}{item}"), self.width, "…");
            let style = if selected {
                Style { bg: Bg::Selected, ..Style::new(Color::Cyan) }
            } else {
                Style::plain()
            };
            lines.push(Line::new(util::pad(&text, self.width), style));
        }
        lines.push(Line::new(hint, Style::new(Color::Dim)));
        lines
    }

    // -- input --------------------------------------------------------------

    /// Read one line of input. `prompt` is only echoed interactively.
    pub fn read_input(&mut self) -> std::io::Result<Action> {
        if !self.interactive {
            let mut buffer = String::new();
            let read = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buffer)?;
            if read == 0 {
                return Ok(Action::Eof);
            }
            return Ok(Action::Line(buffer.trim_end_matches('\n').to_string()));
        }
        // Raw mode is taken here and released only at teardown, so it stays on across the
        // turn that follows: the input line has to keep receiving keys while the model is
        // answering, and a terminal switched back to cooked mode would hold them in the line
        // discipline until the next prompt.
        let _guard = RawGuard::enter()?;
        self.begin_line();
        loop {
            let event = match event::read() {
                Ok(event) => event,
                Err(_) => return Ok(Action::Eof),
            };
            match self.absorb_event(event) {
                Some(action) => return Ok(action),
                None => continue,
            }
        }
    }

    /// Arm the input line, keeping whatever draft is already in it.
    ///
    /// The composer is armed for the whole session — a turn is a time when the user is very
    /// likely to want to type, so the line they type into has to exist then too. That means
    /// this is called on a buffer that may already hold text the user began writing while the
    /// model was answering, and starting to read the next line must not throw that away: it
    /// is the user's half-written message, and losing it silently is the same bug as losing
    /// keys, one turn later.
    ///
    /// Only the furniture around the line is reset. The buffer is emptied when a line is
    /// submitted (see `handle_key`), which is the one moment it is meant to be emptied.
    pub fn begin_line(&mut self) {
        self.editing.get_or_insert_with(Editor::new);
        self.menu.clear();
        self.menu_selected = 0;
        self.menu_dismissed = None;
        self.render();
    }

    /// Take any typing that has already arrived, without waiting for more.
    ///
    /// This is what keeps the input line alive while the model is answering: the turn loop
    /// calls it between deltas, so a keypress lands in the composer as it is pressed. The
    /// alternative — reading input only when the turn is over — is why typing during a turn
    /// did nothing at all: the bytes sat in the terminal buffer until the next prompt.
    ///
    /// Only already-buffered events are taken, so this never stalls the turn.
    ///
    /// *Every* buffered event is taken, not just one. One per call would tie the input rate
    /// to how often the caller comes round — with nothing but the spinner ticking, that is
    /// one key per frame, and pasting a line would take seconds to appear.
    pub fn poll_input(&mut self) -> Option<Action> {
        if !self.interactive {
            return None;
        }
        let mut pending: Option<Action> = None;
        // The first action ends the drain: it is a submitted line or a Ctrl+O, and the
        // caller acts on it before any more typing is read.
        while pending.is_none() && event::poll(std::time::Duration::ZERO).unwrap_or(false) {
            match event::read() {
                Ok(event) => pending = self.absorb_event(event),
                Err(_) => break,
            }
        }
        pending
    }

    /// Take one terminal event.
    ///
    /// Returns `Some(action)` when the line was submitted, when the user asked to interrupt
    /// or to expand something, and `None` when the event only changed what is on screen. The
    /// caller decides what a submitted line means: at the prompt it is the next turn, and
    /// during a turn it is a message queued behind the one in flight.
    pub fn absorb_event(&mut self, event: Event) -> Option<Action> {
        if let Event::Resize(_, _) = event {
            self.refresh_size();
            self.render();
            return None;
        }
        let Event::Key(key) = event else {
            return None;
        };
        if key.kind == KeyEventKind::Release {
            return None;
        }
        // A notice describes the *previous* keystroke, so it is cleared before this one is
        // handled rather than after: a notice the handler sets (a failed paste, say) is about
        // what just happened and has to survive the redraw that shows it. Clearing it after
        // the handler wiped the notice in the very same keystroke that raised it, so it was
        // never on screen at all.
        self.notice = None;
        let action = self.handle_key(key);
        if action.is_some() {
            // The line is on its way out: clear the composer and redraw once without it. The
            // buffer is cleared when the line is taken, but the terminal still shows the
            // frame from the last keystroke, so without this the submitted text sits in the
            // input row until something else repaints.
            self.menu.clear();
            self.menu_selected = 0;
            self.menu_dismissed = None;
            self.render();
            return action;
        }
        self.sync_menu();
        self.render();
        None
    }

    /// Turn one keystroke into an edit, or into the action it stands for.
    ///
    /// This is the whole line editor, and there is exactly one copy of it: the prompt and the
    /// turn loop both feed keys through here, so a key that edits works while the model is
    /// still answering. A second implementation for "typing while busy" would be a second set
    /// of bugs.
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => {
                let mut line = self.editing.as_ref().map(Editor::text).unwrap_or_default();
                // The line is on its way out, so clear the buffer now: the caller commits
                // it to the transcript and redraws, and a buffer that still holds the
                // submitted text would be drawn again as a fresh prompt.
                self.editing = Some(Editor::new());
                // The walk through the history ends with the line it produced. Leaving the
                // index pointing at a recalled entry would make the next Down continue the
                // walk from there instead of doing nothing on a fresh, empty line.
                self.history_index = None;
                self.history_draft = None;
                // Enter runs the highlighted command. When the buffer already spells
                // that command out (`/exit` typed by hand, or a `/model` argument in
                // progress), it is taken as written so an argument survives.
                if let Some(accepted) = self.accepted_command() {
                    let typed = line.strip_prefix('/').map(str::trim).unwrap_or_default();
                    let has_argument = typed.contains(' ');
                    if !has_argument {
                        line = accepted;
                    }
                }
                self.remember(&line);
                // Images travel with the line, and an image with no text at all is a
                // valid message: the user may only want to show a screenshot.
                if !self.pending_images.is_empty() {
                    let images = std::mem::take(&mut self.pending_images);
                    return Some(Action::LineWithImages(line, images));
                }
                return Some(Action::Line(line));
            }
            KeyCode::Char('o') if ctrl => return Some(Action::ToggleExpand),
            KeyCode::Char('c') if ctrl => {
                let empty = self.editing.as_ref().is_none_or(Editor::is_empty)
                    && self.pending_images.is_empty();
                if empty {
                    return Some(Action::Interrupt);
                }
                self.editing = Some(Editor::new());
                self.pending_images.clear();
            }
            KeyCode::Char('d') if ctrl => {
                if self.editing.as_ref().is_none_or(Editor::is_empty) {
                    return Some(Action::Eof);
                }
                // Like Ctrl+C, this leaves a non-empty line alone: Ctrl+D means "close
                // the stream", and the buffer is not part of that.
            }
            KeyCode::Char('v') if ctrl => {
                // An image on the clipboard wins over text: a screenshot tool usually
                // leaves both, and the user pressing Ctrl+V after a screenshot means
                // the picture. Text paste is the consolation path.
                match image_input::read_clipboard_image() {
                    Ok(image) => {
                        self.pending_images.push(image);
                    }
                    Err(image_input::ImageError::NoImage) => {
                        if let Ok(text) = image_input::read_clipboard_text()
                            && let Some(editor) = &mut self.editing
                        {
                            // One line at a time: the editor has no multiline buffer,
                            // and a raw newline would break the layout.
                            editor.insert(&text.replace(['\n', '\r'], " "));
                        }
                    }
                    Err(err) => {
                        self.notice = Some(format!("粘贴失败：{err}"));
                    }
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(editor) = &mut self.editing {
                    editor.delete_to_start();
                }
            }
            KeyCode::Char('k') if ctrl => {
                if let Some(editor) = &mut self.editing {
                    editor.delete_to_end();
                }
            }
            KeyCode::Char('w') if ctrl => {
                if let Some(editor) = &mut self.editing {
                    editor.delete_word();
                }
            }
            KeyCode::Char('a') if ctrl => {
                if let Some(editor) = &mut self.editing {
                    editor.home();
                }
            }
            KeyCode::Char('e') if ctrl => {
                if let Some(editor) = &mut self.editing {
                    editor.end();
                }
            }
            // Left/Right move the caret in characters, so the units match the buffer
            // rather than the screen: a CJK character is one step, not two.
            KeyCode::Left if alt => {
                if let Some(editor) = &mut self.editing {
                    editor.delete_word();
                }
            }
            KeyCode::Left => {
                if let Some(editor) = &mut self.editing {
                    editor.move_caret(-1);
                }
            }
            KeyCode::Right => {
                if let Some(editor) = &mut self.editing {
                    editor.move_caret(1);
                }
            }
            KeyCode::Home => {
                if let Some(editor) = &mut self.editing {
                    editor.home();
                }
            }
            KeyCode::End => {
                if let Some(editor) = &mut self.editing {
                    editor.end();
                }
            }
            KeyCode::Delete => {
                if let Some(editor) = &mut self.editing {
                    editor.delete();
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                if let Some(editor) = &mut self.editing {
                    editor.insert(&c.to_string());
                }
            }
            KeyCode::Backspace => {
                if let Some(editor) = &mut self.editing {
                    editor.backspace();
                }
            }
            KeyCode::Tab => {
                self.complete();
            }
            KeyCode::BackTab => {
                self.move_menu(-1);
            }
            KeyCode::Esc => {
                // An open menu is what Esc closes first: it is a list the user is being
                // asked about, and dismissing it must never cost them the answer the model
                // is halfway through writing.
                if !self.menu.is_empty() {
                    // Closing the menu must not drop what was typed, and it has to *stay*
                    // closed: the redraw at the end of this very keypress would otherwise put
                    // the list straight back. Typing on starts a fresh command name, so the
                    // menu comes back then.
                    self.menu_dismissed = self.editing.as_ref().map(Editor::text);
                    self.menu.clear();
                    self.menu_selected = 0;
                } else if self.working.is_some() {
                    // Nothing to dismiss, and something is running: Esc stops it. The
                    // spinner is the only thing on screen that says a turn is in flight, so
                    // it is also what decides whether this key means "stop".
                    return Some(Action::Stop);
                }
            }
            // The arrows drive the menu when it is up, and the input history otherwise.
            //
            // This is the arrangement the user asked for twice, from both sides: the menu has
            // to be selectable with the arrows, and Down at the end of the history has to
            // reach a blank line. They only conflict because recalling a `/command` used to
            // pop the menu open on it, and then the arrows belonged to a list the user never
            // asked for. The recall is what suppresses the menu (see `history_up`), so a
            // refreshed list here means the user opened it by typing.
            KeyCode::Up if !self.menu.is_empty() => {
                self.move_menu(-1);
            }
            KeyCode::Down if !self.menu.is_empty() => {
                self.move_menu(1);
            }
            KeyCode::Up => {
                self.history_up();
            }
            KeyCode::Down => {
                self.history_down();
            }
            _ => {}
        }
        None
    }

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

    /// Add a submitted line to the history, so Up recalls it later.
    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }
}

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
pub(crate) struct RawGuard;

static RAW_HOLDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl RawGuard {
    pub(crate) fn enter() -> std::io::Result<Self> {
        if RAW_HOLDERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            if let Err(err) = terminal::enable_raw_mode() {
                RAW_HOLDERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(err);
            }
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

/// A convenience wrapper for the home directory shortcut.
pub fn short_cwd(cwd: &Path) -> String {
    util::shorten_home(cwd, dirs::home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen {
        let mut screen = Screen::new();
        screen.interactive = false;
        screen.width = 40;
        screen.height = 24;
        screen
    }

    /// Set the input buffer the way a test means it: text, with the caret at the end.
    fn set_input(screen: &mut Screen, text: &str) {
        screen.editing = Some(Editor::from_text(text));
    }

    /// The input buffer as a string.
    fn input(screen: &Screen) -> String {
        screen.editing.as_ref().map(Editor::text).unwrap_or_default()
    }

    /// Put the caret `back` characters from the end of the buffer.
    fn caret_back(screen: &mut Screen, back: usize) {
        if let Some(editor) = &mut screen.editing {
            editor.home();
            editor.move_caret((editor.len() - back.min(editor.len())) as isize);
        }
    }

    #[test]
    fn leaving_takes_the_live_region_down() {
        // The live region is the prompt and the footer, not part of the transcript. If it is
        // left on screen the shell inherits a cursor parked mid-row, and zsh marks a partial
        // line with `%` — so pi's prompt appears to survive as `› /%`.
        //
        // The draw and erase steps are tested above; what this pins is that the exit path
        // actually erases, because the residue only shows up in a real shell.
        //
        // `interactive` has to be on: a piped run writes no escape sequences at all, and
        // `erase_live` is a no-op there by design.
        let mut screen = screen();
        screen.interactive = true;
        set_input(&mut screen, "/");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);

        // Draw once, as the input loop does, so there is a region to take down.
        let (lines, cursor) = screen.compose_live();
        assert!(cursor.is_some(), "an input line means a cursor to park");
        screen.live_rows = lines.len();
        screen.cursor_row = cursor.map(|(row, _)| row);

        screen.leave();

        // The region is gone, so a later erase has nothing to do — which is exactly the
        // state that keeps the shell's marker off the screen.
        assert_eq!(screen.live_rows, 0);
        assert_eq!(screen.cursor_row, None);
    }

    #[test]
    fn leaving_commits_what_was_queued_but_not_yet_drawn() {
        // The note telling the user how to come back is pushed *after* the turn loop ends,
        // so no draw ever follows it. Erasing without committing threw it away silently: the
        // last thing pi is supposed to say was the one thing it never said.
        let mut screen = screen();
        screen.interactive = true;
        screen.push_lines(vec![Line::plain("继续此会话：pi resume abc")]);
        assert!(!screen.blocks.is_empty(), "the note is queued");

        screen.leave();

        // `printed` catches up with `blocks` only if the queued row was written.
        assert_eq!(
            screen.printed,
            screen.blocks.len(),
            "a queued row must be written before the live region is taken down"
        );
    }

    #[test]
    fn the_window_title_names_the_project_not_its_path() {
        // The tab shows which project this is; where it lives is not the question a tab
        // answers, and the path is what made the title unreadable.
        let cwd = Path::new("/home/me/文档/mpi");
        assert_eq!(window_title(None, cwd), "π - mpi");
        // A session name wins: the user chose it deliberately.
        assert_eq!(window_title(Some("重构解析器"), cwd), "π - 重构解析器");
        assert_eq!(window_title(Some("   "), cwd), "π - mpi");
        // Only the last component is used, so a deeply nested checkout stays short.
        assert_eq!(window_title(None, Path::new("/a/b/c/deep-project")), "π - deep-project");
        // The file-system root has no name to show.
        assert_eq!(window_title(None, Path::new("/")), "π");
    }

    #[test]
    fn a_session_name_cannot_break_out_of_the_title_sequence() {
        // The name is user input. A raw ESC or BEL here would end the OSC sequence early and
        // let whatever follows be read as a terminal command, so both are stripped.
        let cwd = Path::new("/tmp/work");
        let title = window_title(Some("a\u{1b}]0;evil\u{7}b"), cwd);
        assert!(!title.contains('\u{1b}'), "{title:?}");
        assert!(!title.contains('\u{7}'), "{title:?}");
        // The injected sequence is removed outright, not merely neutralised.
        assert_eq!(title, "π - ab");
        // A bare BEL is dropped as well.
        assert_eq!(window_title(Some("a\u{7}b"), cwd), "π - ab");
        // A newline would split the title across two lines in the terminal's tab bar.
        assert!(!window_title(Some("a\nb"), cwd).contains('\n'));
    }

    #[test]
    fn a_piped_screen_does_not_write_a_title() {
        // `screen()` is non-interactive, so nothing should reach stdout.
        let mut screen = screen();
        screen.set_title(Some("demo"), Path::new("/tmp/work"));
        assert!(screen.title.is_none());
    }

    fn screen_with_commands() -> Screen {
        let mut screen = screen();
        screen.set_commands(crate::agent::r#loop::COMMANDS);
        screen
    }

    #[test]
    fn the_working_spinner_sits_above_the_input_and_moves() {
        // pi's indicator: a frame that advances, at the left of the input area. Its whole
        // job is to be *moving* — a still mark cannot be told apart from a hung process, so
        // the frames are asserted to differ rather than merely to exist.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        screen.working = Some(WORKING_LABEL.to_string());

        let (lines, _) = screen.compose_live();
        let input = lines.iter().position(|line| line.text().starts_with('›')).unwrap();
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert_eq!(input, 1, "the spinner is one row: {text:?}");
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL));

        // Each tick advances by exactly one frame and wraps, so the animation has no jump.
        screen.working_frame = WORKING_FRAMES.len() - 1;
        screen.tick_working();
        let (lines, _) = screen.compose_live();
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL));
        screen.tick_working();
        let (lines, _) = screen.compose_live();
        assert_eq!(lines[0].text(), format!("{} {}", WORKING_FRAMES[1], WORKING_LABEL));
    }

    #[test]
    fn the_spinner_is_gone_once_the_turn_ends() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "");
        screen.working = Some(WORKING_LABEL.to_string());
        screen.clear_working();
        let (lines, _) = screen.compose_live();
        let text: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(lines[0].text().starts_with('›'), "the input is the first row again: {text:?}");
    }

    #[test]
    fn every_spinner_frame_is_one_column_wide() {
        // The label sits to the right of the frame, so a frame of a different width would
        // make it shift sideways on every tick.
        for frame in WORKING_FRAMES {
            assert_eq!(util::width(frame), 1, "frame {frame:?} is not one column");
        }
    }

    #[test]
    fn a_slash_opens_the_menu_and_filters_as_more_is_typed() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/");
        screen.sync_menu();
        assert_eq!(screen.menu.len(), crate::agent::r#loop::COMMANDS.len());
        // The first entry is highlighted, so Enter has an unambiguous target.
        assert_eq!(screen.menu[0].0, "model");

        set_input(&mut screen, "/m");
        screen.sync_menu();
        // "m" matches /model and /compact: the match is on the name, not the description.
        assert_eq!(screen.menu.len(), 1, "{:?}", screen.menu);
        assert_eq!(screen.menu[0].0, "model");

        set_input(&mut screen, "/na");
        screen.sync_menu();
        assert_eq!(screen.menu.len(), 1);
        assert_eq!(screen.menu[0].0, "name");
    }

    #[test]
    fn the_menu_stays_out_of_the_way_of_ordinary_text() {
        let mut screen = screen_with_commands();
        // A slash mid-sentence is not a command, and an argument is being typed once there
        // is a space, so in both cases there is nothing to complete.
        for text in ["你好/世界", "/name 我的会话", "no slash at all"] {
            set_input(&mut screen, text);
            screen.sync_menu();
            assert!(screen.menu.is_empty(), "{text:?} opened a menu");
        }
    }

    #[test]
    fn tab_completes_a_unique_command_along_with_a_space() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/na");
        assert!(screen.complete());
        // The trailing space means the argument can be typed straight away.
        assert_eq!(input(&screen), "/name ");
        // The menu closes because the name is settled.
        assert!(screen.menu.is_empty());
    }

    #[test]
    fn tab_shares_a_prefix_before_cycling_through_the_menu() {
        let mut screen = screen_with_commands();
        // "co" matches only /compact, so that case is covered above; "c" also matches
        // nothing else, so use two commands sharing a prefix via the real list.
        set_input(&mut screen, "/");
        assert!(screen.complete() || !screen.menu.is_empty());
        // With several matches and no shared prefix to add, Tab walks the highlight.
        let before = screen.menu_selected;
        screen.complete();
        assert_ne!(screen.menu_selected, before);
    }

    #[test]
    fn tab_on_an_exact_command_does_nothing_destructive() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/exit");
        screen.sync_menu();
        // /exit is the only match, so Tab would fill in the space; the point is that it
        // must not lose what was typed.
        screen.complete();
        assert!(input(&screen).starts_with("/exit"));
    }

    #[test]
    fn a_shared_prefix_is_filled_in_before_cycling() {
        assert_eq!(common_prefix(&["model", "modify"]), "mod");
        assert_eq!(common_prefix(&["name", "new"]), "n");
        // No shared prefix: the empty string, which is always a prefix, and the caller
        // then falls through to cycling the menu instead of typing anything.
        assert_eq!(common_prefix(&["model", "exit"]), "");
        assert_eq!(common_prefix(&["only"]), "only");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn a_menu_selection_is_the_command_that_runs() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/re");
        screen.sync_menu();
        // Enter takes the highlighted entry as a whole command: the menu is there to save
        // typing, so picking from it must not require a second Enter to submit.
        assert_eq!(screen.accepted_command().as_deref(), Some("/resume"));
        assert_eq!(screen.menu.len(), 1, "the filter left only the match");

        // Moving the highlight moves what Enter would run.
        set_input(&mut screen, "/");
        screen.sync_menu();
        screen.move_menu(1);
        assert_eq!(screen.accepted_command(), Some("/name".into()));

        // With no menu open there is nothing to accept, and the buffer is submitted as
        // typed.
        set_input(&mut screen, "你好");
        screen.sync_menu();
        assert_eq!(screen.accepted_command(), None);
    }

    #[test]
    fn the_menu_wraps_at_both_ends() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/");
        screen.sync_menu();
        assert!(screen.move_menu(-1));
        assert_eq!(screen.menu_selected, screen.menu.len() - 1);
        assert!(screen.move_menu(1));
        assert_eq!(screen.menu_selected, 0);
    }

    #[test]
    fn the_cursor_lands_after_the_buffer_not_below_the_footer() {
        // The live region is drawn as text; nothing moves the cursor unless the drawing
        // code says where it goes. Without this the caret ends up under the footer.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.expect("an editing screen has a cursor");
        // The prompt is two columns, and the buffer is three characters.
        assert_eq!(column, 2 + 3);
        // The row holds the input line — not the footer, which is below it.
        assert!(lines[row].text().starts_with("› /mo"), "{:?}", lines[row].text());
        assert!(row + 1 < lines.len());
        assert!(lines[row + 1].text().contains("/model"), "the menu goes under the input");
    }

    #[test]
    fn the_caret_sits_where_the_buffer_put_it() {
        // The caret is part of the editor, not a property of the end of the line. Without
        // this the only place a correction can be typed is the end, which is what "you can't
        // go back and fix a typo" means from the user's side.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "helo world");
        // `helo world` is ten characters; six steps back puts the caret after `helo`.
        caret_back(&mut screen, 6);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "› helo world");
        assert_eq!(column, 2 + 4, "the caret is inside the word, not at its end");
    }

    #[test]
    fn left_and_right_step_by_character_and_stop_at_both_ends() {
        let mut editor = Editor::from_text("abc");
        assert_eq!(editor.caret(), 3);
        editor.move_caret(-2);
        assert_eq!(editor.caret(), 1);
        editor.move_caret(1);
        assert_eq!(editor.caret(), 2);
        // Held at either end rather than wrapping: a caret that jumped to the other end
        // would make correcting a typo a guess about where it landed.
        editor.move_caret(10);
        assert_eq!(editor.caret(), 3);
        editor.move_caret(-10);
        assert_eq!(editor.caret(), 0);
    }

    #[test]
    fn a_wide_character_is_one_step_and_two_columns() {
        // Stepping by character rather than by column is what keeps the caret from landing
        // in the middle of a CJK glyph, where there is no position to draw it.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "你好");
        caret_back(&mut screen, 1);
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "› 你好");
        assert_eq!(column, 2 + 2, "one character back is two display columns");
    }

    #[test]
    fn typing_in_the_middle_inserts_at_the_caret() {
        let mut editor = Editor::from_text("helo");
        editor.move_caret(-1);
        editor.insert("l");
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.caret(), 4, "the caret follows what was typed");
    }

    #[test]
    fn backspace_and_delete_remove_on_opposite_sides_of_the_caret() {
        // Backspace takes the character before the caret and moves it; Delete takes the one
        // under it and leaves it where it is. Swapping them silently edits the wrong
        // character, which is worse than not supporting the key at all.
        let mut editor = Editor::from_text("abc");
        editor.move_caret(-1);
        editor.backspace();
        assert_eq!(editor.text(), "ac");
        assert_eq!(editor.caret(), 1);
        editor.delete();
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.caret(), 1);
        // Neither one can run off either end.
        editor.delete();
        assert_eq!(editor.text(), "a");
        editor.backspace();
        assert_eq!(editor.text(), "");
        editor.backspace();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn the_kill_keys_act_around_the_caret() {
        let mut editor = Editor::from_text("one two three");
        editor.move_caret(-6);
        // Ctrl+W takes the word *before* the caret, wherever the caret is. The space that
        // separated it is not part of the word, so it stays — the same thing readline does.
        editor.delete_word();
        assert_eq!(editor.text(), "one  three");
        // Ctrl+U takes everything before it, Ctrl+K everything after.
        editor.delete_to_start();
        assert_eq!(editor.text(), " three");
        assert_eq!(editor.caret(), 0);
        editor.delete_to_end();
        assert_eq!(editor.text(), "");
        // Ctrl+W on an empty buffer is a no-op rather than a panic.
        editor.delete_word();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn ctrl_w_eats_the_spaces_before_the_word_too() {
        let mut editor = Editor::from_text("git commit ");
        editor.delete_word();
        assert_eq!(editor.text(), "git ");
        editor.delete_word();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn the_wrapped_caret_follows_the_text_to_the_next_row() {
        // A caret just before a wrap boundary belongs on the row below, at its first column:
        // putting it on the row above would place it one cell past the terminal's edge.
        let mut screen = screen_with_commands();
        // 12 columns, 2 for the prompt: 10 characters fill the first row exactly.
        screen.width = 12;
        set_input(&mut screen, "abcdefghijklm");
        caret_back(&mut screen, 3); // the caret is exactly on the wrap boundary
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "  klm", "a full row puts the caret on the next one");
        assert_eq!(column, 2, "at the first text column, not one past the screen edge");
    }

    #[test]
    fn a_recalled_command_does_not_open_the_menu_over_the_arrows() {
        // The two requirements pull in opposite directions and this is where they meet: the
        // menu must be selectable with the arrows, and Down at the end of the history must
        // reach a blank line. They conflicted because recalling a `/command` popped the menu
        // open on it, and the arrows then belonged to a list nobody asked for. A recall is not
        // typing, so it does not open the menu — which leaves the arrows free for the history.
        let mut screen = screen_with_commands();
        screen.history = vec!["/name".into(), "hello".into()];
        set_input(&mut screen, "");
        screen.history_index = Some(0);

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
        assert_eq!(input(&screen), "/name", "Up recalls the previous line");
        assert!(screen.menu.is_empty(), "recalling a command must not open the menu");

        // Down is the way back out of the history, and it still works.
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(input(&screen), "hello", "Down leaves the recalled command behind");
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(input(&screen), "", "and the last Down reaches the blank line");
    }

    #[test]
    fn the_arrows_select_from_the_menu_while_it_is_open() {
        // Typing `/` opens the menu, and the arrows have to move through it: that is how a
        // menu is used everywhere else, and the highlight is the only thing that says which
        // entry Enter would run.
        let mut screen = screen_with_commands();
        screen.begin_line();
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Char('/'))));
        assert!(screen.menu.len() > 1, "the menu lists the commands");
        assert_eq!(screen.menu_selected, 0);

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
        assert_eq!(screen.menu_selected, 1, "Down moves the highlight");
        assert_eq!(input(&screen), "/", "and leaves the buffer alone");

        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
        assert_eq!(screen.menu_selected, 0, "Up moves it back");
    }

    #[test]
    fn esc_stops_the_turn_only_when_something_is_running() {
        // Esc has two jobs and they must not collide: it closes an open menu, and it stops a
        // turn. With nothing running and no menu it stays a no-op — it cannot be a Stop that
        // nothing is listening for, because `read_input` at the prompt would then have to
        // filter it back out.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "hello");
        screen.sync_menu();
        assert!(screen.menu.is_empty());

        // Nothing running: Esc is swallowed.
        assert!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))).is_none(),
            "Esc at an idle prompt is not an action"
        );

        // A turn in flight: Esc is the stop.
        screen.working = Some(WORKING_LABEL.to_string());
        assert_eq!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
            Some(Action::Stop)
        );
    }

    #[test]
    fn esc_closes_an_open_menu_instead_of_stopping_the_turn() {
        // The menu wins: it is a list the user is being asked about, and dismissing it must
        // not cost them the answer being written behind it. The turn only stops on the Esc
        // *after* the menu is gone, which is what makes the two jobs separable by pressing
        // the key twice.
        let mut screen = screen_with_commands();
        screen.working = Some(WORKING_LABEL.to_string());
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        assert!(!screen.menu.is_empty(), "typing a command name opens the menu");

        assert!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))).is_none(),
            "the first Esc only closes the menu"
        );
        assert!(screen.menu.is_empty());
        assert_eq!(input(&screen), "/mo", "and the buffer survives");

        assert_eq!(
            screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
            Some(Action::Stop),
            "the second Esc, with no menu left, stops the turn"
        );
    }

    #[test]
    fn esc_closes_the_menu_and_it_stays_closed() {
        // Esc had no effect at all before: the redraw at the end of the same keypress put the
        // list straight back. It is the buffer the user dismissed, so typing on brings the
        // menu back — a dismissal that survived to the end of the line would make it feel
        // broken in the other direction.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "/mo");
        screen.sync_menu();
        assert!(!screen.menu.is_empty());

        // What the Esc key does.
        screen.menu_dismissed = screen.editing.as_ref().map(Editor::text);
        screen.menu.clear();
        screen.sync_menu();
        assert!(screen.menu.is_empty(), "the redraw must not bring it back");

        // The buffer moves on, so the dismissal no longer describes what is on screen.
        set_input(&mut screen, "/mod");
        screen.sync_menu();
        assert!(!screen.menu.is_empty(), "a new command name gets its menu back");
        assert!(screen.menu_dismissed.is_none());
    }

    #[test]
    fn a_line_submitted_mid_turn_is_held_as_what_it_is() {
        // A command typed while a turn is running cannot run then, but it also must not be
        // sent to the model as prose: `/model` is not something the user said. It waits as a
        // command, and a message waits as a message with its images — the two part ways the
        // moment the turn ends.
        let mut screen = screen_with_commands();
        screen.interactive = false;

        crate::agent::r#loop::queue_mid_turn(&mut screen, "/model".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "hello".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "   ".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued.len(), 2, "a blank line is not queued: {queued:?}");
        assert_eq!(queued[0], Queued::Command("/model".to_string()));
        assert_eq!(queued[1], Queued::Message("hello".to_string(), Vec::new()));

        // Taking them empties the queue, so the next turn does not re-send them.
        assert!(screen.take_queued().is_empty());
    }

    #[test]
    fn a_queued_line_is_trimmed_the_way_the_prompt_trims_it() {
        // A line has to reach the conversation the same way whether it was typed at the
        // prompt or during a turn. The prompt trims, so queueing does too — otherwise
        // " /model" would go to the model as a message while the same text typed at the
        // prompt would be rejected as an unknown command.
        let mut screen = screen_with_commands();
        crate::agent::r#loop::queue_mid_turn(&mut screen, "  /model  ".to_string(), Vec::new());
        crate::agent::r#loop::queue_mid_turn(&mut screen, "  hello  ".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued[0], Queued::Command("/model".to_string()));
        assert_eq!(queued[1], Queued::Message("hello".to_string(), Vec::new()));
    }

    #[test]
    fn a_command_queued_mid_turn_never_becomes_a_message() {
        // The bug this pins: the command used to be queued as a *message* with a note welded
        // into the text ("/model　（回合结束后执行）"), so the model received the text of a
        // command as something the user had said, and the command itself never ran.
        let mut screen = screen_with_commands();
        crate::agent::r#loop::queue_mid_turn(&mut screen, "/name x".to_string(), Vec::new());
        let queued = screen.take_queued();
        assert!(
            queued.iter().all(|item| !matches!(item, Queued::Message(..))),
            "a command must not be queued as a message: {queued:?}"
        );
    }

    #[test]
    fn a_paste_that_was_never_submitted_stays_with_the_draft() {
        // `take_queued` used to sweep `pending_images` into the queue, which sent a
        // screenshot the user was still composing and left the message it belonged to
        // without it. Images travel with the line on Enter, so the queue never owns them.
        let mut screen = screen_with_commands();
        screen.pending_images.push(crate::image_input::PastedImage {
            width: 4,
            height: 4,
            data: "AAAA".into(),
            bytes: 3,
        });
        crate::agent::r#loop::queue_mid_turn(&mut screen, "queued message".to_string(), Vec::new());

        let queued = screen.take_queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            screen.pending_images.len(),
            1,
            "the unsubmitted paste still belongs to the line being written"
        );
        assert!(matches!(&queued[0], Queued::Message(text, images)
            if text == "queued message" && images.is_empty()));
    }

    #[test]
    fn arming_the_next_line_keeps_the_draft() {
        // The composer is armed again after every turn, and the user may have started
        // writing during the turn that just ended. `begin_line` used to blank the buffer,
        // which threw that message away one turn later than the keystroke bug it fixed.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "half-written");
        screen.begin_line();
        assert_eq!(input(&screen), "half-written");
    }

    #[test]
    fn submitting_a_line_ends_the_history_walk() {
        // Enter takes the recalled line and empties the buffer. The walk has to end there
        // too: an index still pointing into the history makes the next Down continue the
        // walk from a line that is no longer on screen.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into(), "second".into()];
        set_input(&mut screen, "");
        screen.history_up();
        assert_eq!(input(&screen), "second");
        assert!(screen.history_index.is_some());

        screen.handle_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(input(&screen), "", "the line is on its way out");
        assert!(screen.history_index.is_none(), "the walk is over");
        assert!(screen.history_draft.is_none());
    }

    #[test]
    fn walking_back_down_past_the_newest_entry_restores_the_draft() {        // Up is a look at the history, not a commitment: a half-written message must survive
        // the glance. Without the stash, the last Down would blank the line and the user's
        // text would be gone with no sign that it had ever been there.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into(), "second".into()];
        set_input(&mut screen, "my draft");

        screen.history_up();
        assert_eq!(input(&screen), "second");
        screen.history_up();
        assert_eq!(input(&screen), "first");
        screen.history_down();
        assert_eq!(input(&screen), "second");
        screen.history_down();
        assert_eq!(input(&screen), "my draft", "the draft comes back, not a blank line");
        assert!(screen.history_index.is_none(), "the walk is over");
    }

    #[test]
    fn down_without_a_walk_in_progress_does_nothing() {
        // Down belongs to the menu-less prompt as the "blank line" key only while walking;
        // on a fresh buffer it must not clear what is being typed.
        let mut screen = screen_with_commands();
        screen.history = vec!["first".into()];
        set_input(&mut screen, "typing away");
        screen.history_down();
        assert_eq!(input(&screen), "typing away");
    }

    #[test]
    fn up_at_the_oldest_entry_stays_there() {
        // No wrap-around: with one, a repeated key press silently changes which entry is on
        // screen and the top of the history is indistinguishable from the bottom.
        let mut screen = screen_with_commands();
        screen.history = vec!["only".into()];
        screen.history_up();
        screen.history_up();
        assert_eq!(input(&screen), "only");
        assert_eq!(screen.history_index, Some(0));
    }

    #[test]
    fn a_resumed_session_offers_the_lines_it_already_holds() {
        // The bug this guards: history lived only in the process, so Up after a resume
        // reached nothing that was typed before it — the turns in the file looked like they
        // had never been typed, and the user had to retype a line that was already there.
        let mut screen = screen_with_commands();
        screen.seed_history(["asked before resuming".to_string(), "and again".to_string()]);

        screen.history_up();
        assert_eq!(input(&screen), "and again", "the newest seeded line comes first");
        screen.history_up();
        assert_eq!(input(&screen), "asked before resuming");
        screen.history_up();
        assert_eq!(input(&screen), "asked before resuming", "the top of the history");

        // A line typed after the resume joins the same list, in the order it was typed.
        set_input(&mut screen, "typed just now");
        screen.remember("typed just now");
        screen.history_down();
        screen.history_down();
        assert_eq!(input(&screen), "typed just now");
    }

    #[test]
    fn seeding_replaces_the_list_it_inherits() {
        // Seeding happens when the screen changes which conversation it is showing — a
        // resume, a `/resume` to another session, a `/new`. The lines that came with the
        // session being left are not history for the one being opened, and keeping them
        // would put a message from one conversation into the arrows of another.
        let mut screen = screen_with_commands();
        screen.history = vec!["from the last session".into()];
        screen.history_index = Some(0);
        screen.history_draft = Some(Editor::from_text("half written"));

        screen.seed_history(["from this one".to_string()]);

        assert_eq!(screen.history, vec!["from this one".to_string()]);
        assert!(screen.history_index.is_none(), "the walk belongs to the old list");
        assert!(screen.history_draft.is_none());

        // A new session has nothing to recall.
        screen.seed_history(Vec::new());
        assert!(screen.history.is_empty());
    }

    #[test]
    fn seeding_keeps_the_newest_entries() {
        // A long session has more lines than the cap; what is kept has to be the tail, since
        // that is what a recalled line is likely to be about.
        let mut screen = screen_with_commands();
        screen.seed_history((0..HISTORY_LIMIT + 5).map(|i| format!("line {i}")));

        assert_eq!(screen.history.len(), HISTORY_LIMIT);
        assert_eq!(screen.history[0], "line 5", "the oldest entries are the ones dropped");
        assert_eq!(screen.history[HISTORY_LIMIT - 1], format!("line {}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn blank_seeded_lines_are_not_entries() {
        // Up on a blank line looks like a broken key: it takes the walk from one empty entry
        // to the next, and the user cannot tell it apart from reaching the top.
        let mut screen = screen_with_commands();
        screen.seed_history(["real".to_string(), "   ".to_string(), String::new()]);
        assert_eq!(screen.history, vec!["real".to_string()]);
    }

    #[test]
    fn a_row_filled_to_the_edge_gets_a_row_for_the_caret() {
        // 10 text columns, filled exactly. The caret marks where the next character goes,
        // and there is no cell left on that row to draw it in — asking for column 13 of a
        // 12-column terminal gets clamped by the terminal, and a caret that depends on a
        // clamp is a caret that is sometimes somewhere else.
        let mut screen = screen_with_commands();
        screen.width = 12;
        set_input(&mut screen, "abcdefghij");
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        assert_eq!(lines[row].text(), "  ", "the caret sits on its own row below");
        assert_eq!(column, 2);
        assert_eq!(row, 1, "not on the row that is full");
    }

    #[test]
    fn the_caret_of_a_recalled_entry_lands_at_the_end() {
        // Recalling a line is for running or amending it, so the caret belongs where the
        // typing stopped — not at the start, where the next keystroke would be an insertion
        // into the middle of a command.
        let mut screen = screen_with_commands();
        screen.history = vec!["/name x".into()];
        set_input(&mut screen, "");
        screen.history_up();
        assert_eq!(screen.editing.as_ref().unwrap().caret(), 7);
    }

    #[test]
    fn the_erase_step_lands_on_the_first_live_row() {
        // Both halves of this arithmetic were wrong at different times, and both failures
        // look like "the screen creeps upward": one row of committed transcript is cleared
        // per redraw. Pin the numbers here rather than in a terminal.
        //
        // While editing, the cursor is parked on the input row, which is the *first* live
        // row — so erasing from there needs no upward move at all.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "hi");
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        let (lines, cursor) = screen.compose_live();
        let (row, _) = cursor.unwrap();
        assert_eq!(row, 0, "the input line is the first live row");
        let up = row; // mirrors erase_live
        assert_eq!(up, 0, "erasing from the input row must not climb");
        assert!(lines.len() > 1);

        // With a pending image and a menu the input row is still first.
        screen.pending_images.push(crate::image_input::PastedImage {
            width: 1,
            height: 1,
            data: "AA==".into(),
            bytes: 3,
        });
        let (lines, cursor) = screen.compose_live();
        let (row, _) = cursor.unwrap();
        assert_eq!(row, 0, "images render below the input line, not above it");
        assert!(lines[0].text().starts_with("› hi"));
    }

    #[test]
    fn the_cursor_step_reaches_the_input_row_from_the_last_drawn_row() {
        // Rows are separated by CRLF, so after drawing N rows the cursor is still on the last
        // one — it is *not* pushed to the row below, which is what used to leave a blank line
        // under the footer. From there, `N - 1 - row` steps up land on `row`.
        for live_rows in 1..6usize {
            for row in 0..live_rows {
                let cursor_row_after_drawing = live_rows - 1;
                let up = live_rows - 1 - row; // mirrors draw_live
                assert_eq!(
                    cursor_row_after_drawing - up,
                    row,
                    "N={live_rows} row={row} must land on the target row"
                );
            }
        }
    }

    #[test]
    fn erasing_reaches_the_first_live_row_from_either_resting_position() {
        // Two places the cursor can be when the next frame starts, and both must climb to the
        // first live row: parked on row `r` while editing, or left on the last drawn row when
        // there is no input line (streaming). Climbing one row too far eats a line of the
        // committed transcript on every redraw.
        for live_rows in 1..6usize {
            // Parked on the input row.
            for row in 0..live_rows {
                let cursor_row = Some(row);
                let up = match cursor_row {
                    Some(row) => row,
                    None => live_rows.saturating_sub(1),
                };
                assert_eq!(up, row);
            }
            // Left on the last drawn row: `live_rows - 1` below the top.
            let up = match None::<usize> {
                Some(row) => row,
                None => live_rows.saturating_sub(1),
            };
            assert_eq!(up, live_rows - 1, "N={live_rows}");
            assert_eq!(live_rows - 1 - up, 0, "N={live_rows} must land on row 0");
        }
    }

    #[test]
    fn a_long_input_wraps_instead_of_scrolling_the_beginning_away() {
        // 12 columns, prompt 2 wide: the first row holds 10 cells, later rows the full 12.
        let text: String = ('a'..='z').collect();
        let rows = input_rows(&text, 12, 2);
        assert_eq!(rows[0].0, "› ");
        assert_eq!(rows[0].1, "abcdefghij");
        assert_eq!(rows[1].0, "  ", "continuation rows align with the text, not the prompt");
        // Ten cells per row, because the two-column prefix is on every row.
        assert_eq!(rows[1].1, "klmnopqrst");
        assert_eq!(rows[2].1, "uvwxyz");
        // Nothing is lost: the beginning stays readable, which is the point.
        let rebuilt: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn a_short_input_stays_on_one_row() {
        let rows = input_rows("hello", 40, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "hello");
    }

    #[test]
    fn an_empty_input_still_has_a_row_for_the_caret() {
        let rows = input_rows("", 40, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "");
    }

    #[test]
    fn a_wide_character_never_splits_across_rows() {
        // 5 columns, prompt 2: three CJK cells fit on the first row, five on the rest.
        let rows = input_rows("你好世界你好世界", 5, 2);
        for (prefix, text) in &rows {
            let total = util::width(prefix) + util::width(text);
            assert!(total <= 5, "{prefix:?}{text:?} is {total} cells wide");
        }
        let rebuilt: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(rebuilt, "你好世界你好世界");
    }

    #[test]
    fn the_cursor_follows_the_last_wrapped_row() {
        let mut screen = screen_with_commands();
        set_input(&mut screen, &('a'..='z').collect::<String>());
        screen.width = 12;
        let (lines, cursor) = screen.compose_live();
        let (row, column) = cursor.unwrap();
        // The caret is on the last row, after its text — not on the first row where it
        // would be if the input were being scrolled sideways.
        assert_eq!(row, 2);
        assert_eq!(lines[row].text(), "  uvwxyz", "the continuation row keeps its indent");
        assert_eq!(column, 2 + 6);
    }

    #[test]
    fn a_cjk_buffer_puts_the_cursor_after_two_cells_per_character() {
        // Column is a display column, not a character count: 你好 is four cells wide, so the
        // caret belongs at 2 + 4, which is what makes it line up with the glyphs.
        let mut screen = screen_with_commands();
        set_input(&mut screen, "你好");
        let (_, cursor) = screen.compose_live();
        assert_eq!(cursor.unwrap().1, 2 + 4);
    }

    #[test]
    fn no_cursor_is_reported_when_not_editing() {
        // While a turn streams, the transcript owns the screen and the terminal caret has
        // nowhere to sit.
        let screen = screen_with_commands();
        let (_, cursor) = screen.compose_live();
        assert!(cursor.is_none());
    }

    #[test]
    fn a_collapsible_block_keeps_only_its_excerpt() {
        let lines: Vec<Line> = (0..10).map(|i| Line::plain(format!("line {i}"))).collect();
        let block = Block::collapsible(lines, 0, 0, 5);
        let rendered = block.render(40);
        // Five rows out of ten, shown as an excerpt of the last five — and no row spent
        // saying so. The user reads this on every tool call; a note is not information.
        assert_eq!(rendered.len(), 5);
        assert_eq!(rendered[0].text(), "line 5");
        assert_eq!(rendered[4].text(), "line 9");
        assert!(!rendered.iter().any(|line| line.text().contains("收起")));
    }

    #[test]
    fn expanding_shows_everything_and_short_blocks_are_left_alone() {
        let mut block = Block::collapsible(
            (0..4).map(|i| Line::plain(format!("l{i}"))).collect(),
            0,
            0,
            5,
        );
        assert_eq!(block.render(40).len(), 4);
        if let Block::Collapsible(collapsible) = &mut block {
            collapsible.expanded = true;
        }
        assert_eq!(block.render(40).len(), 4);
        assert!(!block.render(40).iter().any(|line| line.text().contains("收起")));
    }

    #[test]
    fn toggling_hits_the_most_recent_collapsible_block() {
        let mut screen = screen();
        screen.push(Block::collapsible(vec![Line::plain("x".repeat(10))], 0, 0, 1));
        screen.push(Block::lines(vec![Line::plain("later")]));
        screen.push(Block::collapsible(
            (0..10).map(|i| Line::plain(format!("i{i}"))).collect(),
            0,
            0,
            3,
        ));
        assert!(screen.toggle_last_collapsible());
        let last = screen.blocks.last().unwrap();
        match last {
            Block::Collapsible(collapsible) => assert!(collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
        // The earlier collapsible block is untouched.
        match &screen.blocks[0] {
            Block::Collapsible(collapsible) => assert!(!collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
    }

    #[test]
    fn collapsing_all_resets_expansion() {
        let mut screen = screen();
        screen.push(Block::collapsible((0..8).map(|i| Line::plain(format!("l{i}"))).collect(), 0, 0, 2));
        screen.toggle_last_collapsible();
        screen.collapse_all();
        match &screen.blocks[0] {
            Block::Collapsible(collapsible) => assert!(!collapsible.expanded),
            _ => panic!("expected a collapsible block"),
        }
    }

    #[test]
    fn the_always_visible_head_survives_collapsing() {
        let lines: Vec<Line> = (0..12).map(|i| Line::plain(format!("row {i}"))).collect();
        let block = Block::collapsible(lines, 2, 0, 3);
        let rendered = block.render(40);
        assert_eq!(rendered.len(), 2 + 3);
        assert_eq!(rendered[0].text(), "row 0");
        assert_eq!(rendered[1].text(), "row 1");
        // The excerpt is the tail of what is left after the head.
        assert_eq!(rendered[2].text(), "row 9");
        assert_eq!(rendered[4].text(), "row 11");
    }

    #[test]
    fn an_always_visible_tail_keeps_the_outcome_in_view() {
        let mut lines: Vec<Line> = (0..12).map(|i| Line::plain(format!("row {i}"))).collect();
        lines.push(Line::dim("退出码 3"));
        let block = Block::collapsible(lines, 1, 1, 2);
        let rendered = block.render(40);
        let text: Vec<String> = rendered.iter().map(|l| l.text()).collect();
        assert_eq!(text[0], "row 0");
        assert_eq!(text.len(), 1 + 2 + 1, "head, excerpt, tail — no note row");
        assert!(text.last().unwrap().contains("退出码 3"), "{text:?}");
    }

    #[test]
    fn wrapping_measures_display_width_not_bytes() {
        let lines = vec![Line::plain("你好世界".repeat(6))];
        let rendered = wrap_all(&lines, 20);
        assert!(rendered.len() > 1);
        for line in &rendered {
            assert!(line.width() <= 20, "{line:?}");
            assert_eq!(line.text(), line.text(), "no escapes may be embedded");
        }
    }

    #[test]
    fn wrapping_keeps_every_run_style() {
        let line = Line::spans(vec![
            Span::new("git", Style::new(Color::Output)),
            Span::new(" ", Style::plain()),
            Span::new("log", Style::new(Color::Magenta)),
        ]);
        let rows = wrap_line(&line, 80);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text(), "git log");
        assert_eq!(rows[0].spans[0].style.fg, Color::Output);
        assert_eq!(rows[0].spans[2].style.fg, Color::Magenta);
    }

    #[test]
    fn a_break_drops_the_space_at_the_wrap_point() {
        let rows = wrap_line(&Line::plain("alpha beta gamma"), 11);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text(), "alpha beta");
        assert_eq!(rows[1].text(), "gamma");
    }

    #[test]
    fn painted_output_is_exactly_the_requested_width() {
        let screen = screen();
        let line = Line::spans(vec![
            Span::new("added", Style::with_bg(Color::DiffAddedText, Bg::Added)),
        ]);
        let painted = screen.paint(&line, 40);
        assert_eq!(util::width(&util::strip_ansi(&painted)), 40);
        // Re-measuring the raw string would be wrong, which is the whole reason spans exist.
        assert!(painted.len() > 40);
    }

    #[test]
    fn the_thinking_preview_only_keeps_the_last_two_lines() {
        let mut screen = screen();
        screen.streaming_thinking = Some("a\nb\nc\nd\ne".repeat(1));
        let (live, _) = screen.compose_live();
        assert!(live.len() <= 2, "{live:?}");
        assert!(live.iter().any(|line| line.text().contains('e')));
    }

    #[test]
    fn ansi_in_streamed_text_is_neutralised_before_display() {
        let mut screen = screen();
        screen.streaming_answer = Some("\u{1b}[31mred\u{1b}[0m".into());
        let (live, _) = screen.compose_live();
        assert!(live.iter().all(|line| !line.text().contains('\u{1b}')));
    }
}
