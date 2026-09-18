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
use crate::ui::theme::{Color, Theme};
use crate::util;

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

/// Wrap a group of lines, flattening the result.
pub fn wrap_all(lines: &[Line], width: usize) -> Vec<Line> {
    lines.iter().flat_map(|line| wrap_line(line, width)).collect()
}

/// What the user did at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Line(String),
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
}

impl Screen {
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
        };
        screen.refresh_size();
        screen
    }

    pub fn interactive(&self) -> bool {
        self.interactive
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

    /// Live rows: the prompt while editing, plus the streaming preview and the footer.
    fn compose_live(&self) -> Vec<Line> {
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
            // One row: prompt, the buffer, and a reverse-video cursor block. An input
            // longer than the terminal is scrolled from the left so the caret stays
            // visible, which is what a single-row editor has to do.
            let available = self.width.saturating_sub(4);
            let mut visible = editing.clone();
            while util::width(&visible) > available {
                visible.remove(0);
            }
            lines.push(Line::spans(vec![
                Span::new("› ", Style::new(Color::Cyan)),
                Span::plain(visible),
                Span::new(" ", Style { bg: Bg::Selected, ..Style::plain() }),
            ]));
        }
        lines.extend(self.footer.iter().cloned());
        lines
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

    /// Remove the live region from the screen without touching committed scrollback.
    fn erase_live(&mut self) {
        if !self.interactive || self.live_rows == 0 {
            return;
        }
        let _ = crossterm::execute!(self.out, cursor::MoveToPreviousLine(self.live_rows as u16));
        let _ = crossterm::execute!(self.out, terminal::Clear(terminal::ClearType::FromCursorDown));
        let _ = self.out.flush();
        self.live_rows = 0;
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
        let lines = self.compose_live();
        self.erase_live();
        let mut buffer = String::new();
        for line in &lines {
            buffer.push_str(&self.paint(line, self.width));
            buffer.push_str("\r\n");
        }
        let _ = write!(self.out, "{buffer}");
        let _ = self.out.flush();
        self.live_rows = lines.len();
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
                    let line = self.editing.clone().unwrap_or_default();
                    break Action::Line(line);
                }
                KeyCode::Char('o') if ctrl => break Action::ToggleExpand,
                KeyCode::Char('c') if ctrl => {
                    let empty = self.editing.as_deref().unwrap_or("").is_empty();
                    if empty {
                        break Action::Interrupt;
                    }
                    self.editing = Some(String::new());
                }
                KeyCode::Char('d') if ctrl => {
                    if self.editing.as_deref().unwrap_or("").is_empty() {
                        break Action::Eof;
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
            self.render();
        };
        // The buffer is echoed from `self.editing` while typing; nothing to do here.
        if let Action::Line(ref text) = action
            && !text.trim().is_empty()
        {
            self.history.push(text.clone());
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

/// Leave the terminal clean when mpi exits.
pub fn teardown() {
    let _ = terminal::disable_raw_mode();
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, cursor::Show);
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
    fn a_collapsible_block_keeps_only_its_tail() {
        let lines: Vec<Line> = (0..10).map(|i| Line::plain(format!("line {i}"))).collect();
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
        let live = screen.compose_live();
        assert!(live.len() <= 2, "{live:?}");
        assert!(live.iter().any(|line| line.text().contains('e')));
    }

    #[test]
    fn ansi_in_streamed_text_is_neutralised_before_display() {
        let mut screen = screen();
        screen.streaming_answer = Some("\u{1b}[31mred\u{1b}[0m".into());
        let live = screen.compose_live();
        assert!(live.iter().all(|line| !line.text().contains('\u{1b}')));
    }
}
