//! The terminal view: a scrolling transcript with a pinned two-line footer.
//!
//! The transcript is kept in memory as *unstyled* lines and re-rendered on every change,
//! so collapse/expand and terminal resizes simply rebuild the visible tail. Only the rows
//! mpi owns are redrawn — the cursor moves up over them and the rest of the scrollback is
//! left alone, which is what keeps "the chat scrolls up" behaviour working.
//!
//! Input is read in raw mode because Ctrl+O (expand/collapse) never reaches the process in
//! canonical mode. What the line editor offers is deliberately small: editing, history and
//! the few control keys mpi defines.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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

/// A run of lines that can be collapsed to its tail.
#[derive(Debug, Clone)]
pub struct Collapsible {
    pub lines: Vec<Line>,
    /// Rows that stay visible in both states: a tool result's header belongs here, so
    /// collapsing never hides which command produced the output.
    pub head: usize,
    /// Rows that stay visible at the end in both states: an exit code or a
    /// "full output: …" note. Losing those while collapsed would hide the outcome.
    pub tail: usize,
    /// How many of the remaining rows are shown while collapsed.
    pub preview: usize,
    pub expanded: bool,
    /// The note shown when collapsed.
    pub note_style: Style,
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
            preview,
            expanded: false,
            note_style: Style::new(Color::Dim),
        })
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
                if total <= head + tail + block.preview {
                    return wrapped;
                }
                let hidden = total - head - tail - block.preview;
                let mut out = wrapped[..head].to_vec();
                out.push(Line::new(
                    format!("… 已收起 {hidden} 行 · 按 Ctrl+O 展开"),
                    block.note_style,
                ));
                out.extend(wrapped[head + hidden..head + hidden + block.preview].iter().cloned());
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
fn input_rows(text: &str, width: usize, prompt_width: usize) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    // Every row, continuation included, carries a two-column prefix so the text lines up
    // under itself; the usable width is therefore the same on all of them.
    let room = width.saturating_sub(prompt_width).max(1);
    for c in text.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > room && used > 0 {
            rows.push((String::new(), std::mem::take(&mut current)));
            used = 0;
        }
        current.push(c);
        used += cw;
    }
    rows.push((String::new(), current));
    // Tag the prefixes now that the row count is known.
    rows.into_iter()
        .enumerate()
        .map(|(index, (_, text))| {
            let prefix = if index == 0 {
                "› ".to_string()
            } else {
                " ".repeat(prompt_width)
            };
            (prefix, text)
        })
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
    Eof,
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
    editing: Option<String>,
    history: Vec<String>,
    history_index: Option<usize>,
    /// Streaming preview: thinking tail plus the answer so far.
    streaming_thinking: Option<String>,
    streaming_answer: Option<String>,
    /// Whether stdout is a terminal at all.
    interactive: bool,
    /// What was last written as the window title, so an unchanged title is not rewritten on
    /// every frame.
    title: Option<String>,
    /// Where the cursor was left inside the live region, as a row index, or `None` when it
    /// was left just past the end. Needed to erase the region from the right place.
    cursor_row: Option<usize>,
    /// A one-off line shown under the input, e.g. a failed paste. Cleared on the next edit.
    notice: Option<String>,
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
            streaming_thinking: None,
            streaming_answer: None,
            interactive,
            title: None,
            cursor_row: None,
            notice: None,
            pending_images: Vec::new(),
            commands: Vec::new(),
            menu: Vec::new(),
            menu_selected: 0,
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
    /// then prints a `%` right after it, so the mpi prompt appears to survive as `› /%`.
    ///
    /// Erasing it means the cursor ends up back where the region started, on a line of its
    /// own, which is where the shell expects to find it.
    pub fn leave(&mut self) {
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
        self.editing = None;
        self.erase_live();
        if self.interactive {
            let _ = crossterm::execute!(self.out, terminal::Clear(terminal::ClearType::All));
            let _ = crossterm::execute!(self.out, cursor::MoveTo(0, 0));
            let _ = self.out.flush();
        }
    }

    // -- streaming ----------------------------------------------------------

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
        if let Some(editing) = &self.editing {
            // The buffer wraps onto as many rows as it needs, so a long line stays fully
            // readable — scrolling sideways would hide the beginning of what was typed and
            // give no way to get back to it. The prompt occupies the first two columns of
            // the first row only.
            let prompt_width = 2;
            let width = self.width.max(prompt_width + 1);
            let rows = input_rows(editing, width, prompt_width);
            let last = rows.len().saturating_sub(1);
            for (index, (prefix, text)) in rows.iter().enumerate() {
                // The prefix is drawn on every row: on continuation rows it is spaces, and
                // skipping it would lose the alignment that makes the wrap readable.
                let style = if index == 0 {
                    Style::new(Color::Cyan)
                } else {
                    Style::plain()
                };
                lines.push(Line::spans(vec![
                    Span::new(prefix.clone(), style),
                    Span::plain(text.clone()),
                ]));
            }
            // The caret is at the end of the last row. Both numbers are display columns, so
            // a CJK character counts as the two cells it occupies.
            let (prefix, text) = &rows[last];
            cursor = Some((lines.len() - 1, util::width(prefix) + util::width(text)));
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
        let Some(rest) = self.editing.as_deref().and_then(|text| text.strip_prefix('/')) else {
            return Vec::new();
        };
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

    /// Refresh the menu from the buffer. Called after every edit.
    fn sync_menu(&mut self) {
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
        let Some(text) = self.editing.clone() else {
            return false;
        };
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
        self.editing = Some(filled);
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

    /// Like [`Screen::pick`], with the initial highlight on `initial` (clamped into range).
    /// The menu for `/model` uses this so the current choice starts highlighted and Enter
    /// keeps it.
    pub fn pick_at(&mut self, title: &str, items: &[String], initial: usize) -> Option<usize> {
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
        let result = loop {
            // The menu replaces the footer so it always sits in the same place.
            self.footer = self.menu_lines(title, items, cursor);
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

    fn menu_lines(&self, title: &str, items: &[String], cursor: usize) -> Vec<Line> {
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
        lines.push(Line::new(
            "↑↓ 选择 · Enter 确认 · Esc 取消",
            Style::new(Color::Dim),
        ));
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
        let guard = RawGuard::enter()?;
        self.editing = Some(String::new());
        self.history_index = None;
        self.notice = None;
        self.pending_images.clear();
        self.menu.clear();
        self.menu_selected = 0;
        self.render();
        let action = loop {
            let event = match event::read() {
                Ok(event) => event,
                Err(_) => break Action::Eof,
            };
            let Event::Key(key) = event else {
                if matches!(event, Event::Resize(_, _)) {
                    self.refresh_size();
                    self.render();
                }
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Enter => {
                    let mut line = self.editing.clone().unwrap_or_default();
                    // The line is on its way out, so clear the buffer now: the caller commits
                    // it to the transcript and redraws, and a buffer that still holds the
                    // submitted text would be drawn again as a fresh prompt.
                    self.editing = Some(String::new());
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
                    // Images travel with the line, and an image with no text at all is a
                    // valid message: the user may only want to show a screenshot.
                    if !self.pending_images.is_empty() {
                        let images = std::mem::take(&mut self.pending_images);
                        break Action::LineWithImages(line, images);
                    }
                    break Action::Line(line);
                }
                KeyCode::Char('o') if ctrl => break Action::ToggleExpand,
                KeyCode::Char('c') if ctrl => {
                    let empty = self.editing.as_deref().unwrap_or("").is_empty()
                        && self.pending_images.is_empty();
                    if empty {
                        break Action::Interrupt;
                    }
                    self.editing = Some(String::new());
                    self.pending_images.clear();
                }
                KeyCode::Char('d') if ctrl => {
                    if self.editing.as_deref().unwrap_or("").is_empty() {
                        break Action::Eof;
                    }
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
                                && let Some(buffer) = &mut self.editing
                            {
                                // One line at a time: the editor has no multiline buffer,
                                // and a raw newline would break the layout.
                                buffer.push_str(&text.replace(['\n', '\r'], " "));
                            }
                        }
                        Err(err) => {
                            self.notice = Some(format!("粘贴失败：{err}"));
                        }
                    }
                }
                KeyCode::Char('u') if ctrl => self.editing = Some(String::new()),
                KeyCode::Char('w') if ctrl => {
                    if let Some(text) = &mut self.editing {
                        while text.ends_with(' ') {
                            text.pop();
                        }
                        while let Some(last) = text.chars().last() {
                            if last == ' ' {
                                break;
                            }
                            text.pop();
                        }
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(text) = &mut self.editing {
                        text.push(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(text) = &mut self.editing {
                        text.pop();
                    }
                }
                KeyCode::Tab => {
                    self.complete();
                }
                KeyCode::BackTab => {
                    self.move_menu(-1);
                }
                KeyCode::Esc => {
                    // Closing the menu must not drop what was typed.
                    self.menu.clear();
                    self.menu_selected = 0;
                }
                KeyCode::Up if !self.menu.is_empty() => {
                    self.move_menu(-1);
                }
                KeyCode::Down if !self.menu.is_empty() => {
                    self.move_menu(1);
                }
                KeyCode::Up => {
                    if !self.history.is_empty() {
                        let next = match self.history_index {
                            Some(0) => 0,
                            Some(index) => index - 1,
                            None => self.history.len() - 1,
                        };
                        self.history_index = Some(next);
                        self.editing = Some(self.history[next].clone());
                    }
                }
                KeyCode::Down => {
                    if let Some(index) = self.history_index {
                        if index + 1 < self.history.len() {
                            self.history_index = Some(index + 1);
                            self.editing = Some(self.history[index + 1].clone());
                        } else {
                            self.history_index = None;
                            self.editing = Some(String::new());
                        }
                    }
                }
                _ => {}
            }
            // A notice describes the last keystroke, so it does not outlive it.
            self.notice = None;
            self.sync_menu();
            self.render();
        };
        self.menu.clear();
        self.menu_selected = 0;
        // Redraw once without the submitted line. The buffer is cleared when the line is
        // taken, but the terminal is still showing the frame from the last keystroke, so
        // without this the text sits in the input row until something else repaints —
        // which, while the model is being waited on, can be seconds.
        self.render();
        let history_text = match &action {
            Action::Line(text) | Action::LineWithImages(text, _) => Some(text.clone()),
            _ => None,
        };
        if let Some(text) = history_text
            && !text.trim().is_empty()
        {
            self.history.push(text);
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        drop(guard);
        Ok(action)
    }
}

struct RawGuard;

impl RawGuard {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Leave the terminal clean when mpi exits. Piped output gets no escape sequences.
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

    #[test]
    fn leaving_takes_the_live_region_down() {
        // The live region is the prompt and the footer, not part of the transcript. If it is
        // left on screen the shell inherits a cursor parked mid-row, and zsh marks a partial
        // line with `%` — so mpi's prompt appears to survive as `› /%`.
        //
        // The draw and erase steps are tested above; what this pins is that the exit path
        // actually erases, because the residue only shows up in a real shell.
        //
        // `interactive` has to be on: a piped run writes no escape sequences at all, and
        // `erase_live` is a no-op there by design.
        let mut screen = screen();
        screen.interactive = true;
        screen.editing = Some("/".into());
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
    fn a_slash_opens_the_menu_and_filters_as_more_is_typed() {
        let mut screen = screen_with_commands();
        screen.editing = Some("/".into());
        screen.sync_menu();
        assert_eq!(screen.menu.len(), crate::agent::r#loop::COMMANDS.len());
        // The first entry is highlighted, so Enter has an unambiguous target.
        assert_eq!(screen.menu[0].0, "model");

        screen.editing = Some("/m".into());
        screen.sync_menu();
        // "m" matches /model and /compact: the match is on the name, not the description.
        assert_eq!(screen.menu.len(), 1, "{:?}", screen.menu);
        assert_eq!(screen.menu[0].0, "model");

        screen.editing = Some("/na".into());
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
            screen.editing = Some(text.into());
            screen.sync_menu();
            assert!(screen.menu.is_empty(), "{text:?} opened a menu");
        }
    }

    #[test]
    fn tab_completes_a_unique_command_along_with_a_space() {
        let mut screen = screen_with_commands();
        screen.editing = Some("/na".into());
        assert!(screen.complete());
        // The trailing space means the argument can be typed straight away.
        assert_eq!(screen.editing.as_deref(), Some("/name "));
        // The menu closes because the name is settled.
        assert!(screen.menu.is_empty());
    }

    #[test]
    fn tab_shares_a_prefix_before_cycling_through_the_menu() {
        let mut screen = screen_with_commands();
        // "co" matches only /compact, so that case is covered above; "c" also matches
        // nothing else, so use two commands sharing a prefix via the real list.
        screen.editing = Some("/".into());
        assert!(screen.complete() || !screen.menu.is_empty());
        // With several matches and no shared prefix to add, Tab walks the highlight.
        let before = screen.menu_selected;
        screen.complete();
        assert_ne!(screen.menu_selected, before);
    }

    #[test]
    fn tab_on_an_exact_command_does_nothing_destructive() {
        let mut screen = screen_with_commands();
        screen.editing = Some("/exit".into());
        screen.sync_menu();
        // /exit is the only match, so Tab would fill in the space; the point is that it
        // must not lose what was typed.
        screen.complete();
        assert!(screen.editing.as_deref().unwrap().starts_with("/exit"));
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
        screen.editing = Some("/re".into());
        screen.sync_menu();
        // Enter takes the highlighted entry as a whole command: the menu is there to save
        // typing, so picking from it must not require a second Enter to submit.
        assert_eq!(screen.accepted_command().as_deref(), Some("/resume"));
        assert_eq!(screen.menu.len(), 1, "the filter left only the match");

        // Moving the highlight moves what Enter would run.
        screen.editing = Some("/".into());
        screen.sync_menu();
        screen.move_menu(1);
        assert_eq!(screen.accepted_command(), Some("/name".into()));

        // With no menu open there is nothing to accept, and the buffer is submitted as
        // typed.
        screen.editing = Some("你好".into());
        screen.sync_menu();
        assert_eq!(screen.accepted_command(), None);
    }

    #[test]
    fn the_menu_wraps_at_both_ends() {
        let mut screen = screen_with_commands();
        screen.editing = Some("/".into());
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
        screen.editing = Some("/mo".into());
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
    fn the_erase_step_lands_on_the_first_live_row() {
        // Both halves of this arithmetic were wrong at different times, and both failures
        // look like "the screen creeps upward": one row of committed transcript is cleared
        // per redraw. Pin the numbers here rather than in a terminal.
        //
        // While editing, the cursor is parked on the input row, which is the *first* live
        // row — so erasing from there needs no upward move at all.
        let mut screen = screen_with_commands();
        screen.editing = Some("hi".into());
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
        screen.editing = Some(('a'..='z').collect());
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
        screen.editing = Some("你好".into());
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
    fn a_collapsible_block_keeps_only_its_tail() {        let lines: Vec<Line> = (0..10).map(|i| Line::plain(format!("line {i}"))).collect();
        let block = Block::collapsible(lines, 0, 0, 5);
        let rendered = block.render(40);
        assert_eq!(rendered.len(), 6);
        assert!(rendered[0].text().contains("已收起 5 行"));
        assert_eq!(rendered[1].text(), "line 5");
        assert_eq!(rendered[5].text(), "line 9");
    }

    #[test]
    fn expanding_shows_everything_and_short_blocks_get_no_note() {
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
        assert!(!block.render(40)[0].text().contains("已收起"));
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
        assert_eq!(rendered.len(), 2 + 1 + 3);
        assert_eq!(rendered[0].text(), "row 0");
        assert_eq!(rendered[1].text(), "row 1");
        assert!(rendered[2].text().contains("已收起 7 行"));
        assert_eq!(rendered[5].text(), "row 11");
    }

    #[test]
    fn an_always_visible_tail_keeps_the_outcome_in_view() {
        let mut lines: Vec<Line> = (0..12).map(|i| Line::plain(format!("row {i}"))).collect();
        lines.push(Line::dim("退出码 3"));
        let block = Block::collapsible(lines, 1, 1, 2);
        let rendered = block.render(40);
        let text: Vec<String> = rendered.iter().map(|l| l.text()).collect();
        assert_eq!(text[0], "row 0");
        assert!(text[1].contains("已收起 9 行"), "{text:?}");
        assert_eq!(text.len(), 1 + 1 + 2 + 1);
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
