//! The rendered-line model: styled runs, wrapping, and the collapsible block.
//!
//! Everything here is pure data and pure functions. Nothing touches the terminal, which is
//! what lets the wrapping and the block layout be tested by asserting on values rather than
//! by looking at a screen.
//!
//! Styling is structured rather than embedded as escape codes: a [`Line`] never contains an
//! escape sequence, so wrapping can measure real display width and still keep every run's
//! colour.

use crate::ui::theme::Color;
use crate::util;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bold: bool,
    /// Markdown emphasis. Terminals have had italics since long before this program, and
    /// rendering `*this*` as grey — the previous stand-in — made emphasis look like a hint.
    pub italic: bool,
    pub underline: bool,
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
        Style { fg: Color::Text, bold: false, italic: false, underline: false, bg: Bg::None }
    }

    pub const fn new(fg: Color) -> Self {
        Style { fg, bold: false, italic: false, underline: false, bg: Bg::None }
    }

    pub const fn bold(fg: Color) -> Self {
        Style { fg, bold: true, italic: false, underline: false, bg: Bg::None }
    }

    pub const fn italic(fg: Color) -> Self {
        Style { fg, bold: false, italic: true, underline: false, bg: Bg::None }
    }

    pub const fn with_bg(fg: Color, bg: Bg) -> Self {
        Style { fg, bold: false, italic: false, underline: false, bg }
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
    /// Columns to indent every row *after* the first one when this line wraps.
    ///
    /// Markdown sets it on a list item, so the second row lines up under the text rather
    /// than under the bullet. It lives on the line — rather than being applied by whoever
    /// wrapped it — so wrapping stays idempotent: a line that has already been wrapped and
    /// is wrapped again (at a narrower width, on a resize) keeps its shape instead of
    /// losing the indent on the second pass.
    pub hang: usize,
}

impl Line {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Line { spans: vec![Span::new(text, style)], hang: 0 }
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

    /// Build a line from already-styled runs. Empty runs are dropped, and a line with no
    /// runs left is blank rather than an empty row with a style.
    pub fn spans(spans: Vec<Span>) -> Self {
        let spans: Vec<Span> = spans.into_iter().filter(|span| !span.text.is_empty()).collect();
        if spans.is_empty() {
            Line::blank()
        } else {
            Line { spans, hang: 0 }
        }
    }

    /// [`Line::spans`] with a hanging indent for its continuation rows.
    pub fn hanging(spans: Vec<Span>, hang: usize) -> Self {
        Line { spans, hang }.tidy_spans()
    }

    fn tidy_spans(mut self) -> Self {
        self.spans.retain(|span| !span.text.is_empty());
        if self.spans.is_empty() {
            return Line::blank();
        }
        self
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
///
/// The line's own `hang` is what its continuation rows are indented by, so wrapping an
/// already-wrapped line at a different width gives the same shape rather than a shape that
/// depends on how many times it was wrapped.
pub fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    let hang = line.hang.min(width.saturating_sub(1));
    // The first row has the full width; the rest lose the hang.
    let cont_width = width.saturating_sub(hang).max(1);
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
    // Width available on the row currently being filled.
    let mut limit = width;
    for (c, style) in chars {
        if c == '\n' {
            rows.push(Vec::new());
            used = 0;
            limit = cont_width;
            last_space = None;
            continue;
        }
        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);
        if used > 0 && used + char_width > limit {
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
            limit = cont_width;
        }
        // A regular space is a break opportunity; a non-breaking space is not, which is
        // how a list marker stays with the first word of its item.
        if c == ' ' {
            last_space = Some((rows.len() - 1, rows.last().unwrap().len()));
        }
        used += char_width;
        rows.last_mut().unwrap().push((c, style));
    }
    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let mut spans = coalesce(row);
        // The indent is painted as a plain run, but the rows carry the same `hang` as the
        // row they came from, so a second pass knows what they already represent and does
        // not indent them again.
        if index > 0 && hang > 0 {
            spans.insert(0, Span::plain(" ".repeat(hang)));
        }
        lines.push(Line { spans, hang });
    }
    lines
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
