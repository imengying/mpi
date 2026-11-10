//! Mouse selections use displayed cells, but copy graphemes and source line breaks.
use super::*;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Point {
    pub block: usize,
    pub row: usize,
    pub column: usize,
}

pub(super) struct Selection {
    anchor: Point,
    focus: Point,
    pub dragging: bool,
    pointer: (usize, usize),
    scrolled: Instant,
}

impl Selection {
    pub fn new(point: Point, mouse: event::MouseEvent) -> Self {
        Self {
            anchor: point,
            focus: point,
            dragging: true,
            pointer: (usize::from(mouse.row), usize::from(mouse.column)),
            scrolled: Instant::now(),
        }
    }

    pub fn update(&mut self, point: Point, mouse: event::MouseEvent) {
        self.focus = point;
        self.pointer = (usize::from(mouse.row), usize::from(mouse.column));
        self.dragging = !matches!(mouse.kind, event::MouseEventKind::Up(_));
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }

    pub fn blocks(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.block.min(self.focus.block)..=self.anchor.block.max(self.focus.block)
    }

    pub fn scroll(
        &mut self,
        now: Instant,
        first: usize,
        last: usize,
    ) -> Option<(isize, usize, usize)> {
        if !self.dragging
            || self.is_empty()
            || now.duration_since(self.scrolled) < Duration::from_millis(50)
        {
            return None;
        }
        let (row, column) = self.pointer;
        let delta = if row <= first {
            2
        } else if row >= last {
            -2
        } else {
            return None;
        };
        self.scrolled = now;
        Some((delta, row, column))
    }

    pub fn extend_to(&mut self, point: Point) {
        self.focus = point;
    }

    pub fn dragging_pointer(&self) -> Option<(usize, usize)> {
        self.dragging.then_some(self.pointer)
    }

    fn columns(&self, point: Point) -> Option<std::ops::Range<usize>> {
        if self.is_empty() {
            return None;
        }
        let (start, end) = (self.anchor.min(self.focus), self.anchor.max(self.focus));
        let row = (point.block, point.row);
        if row < (start.block, start.row) || row > (end.block, end.row) {
            return None;
        }
        let first = if row == (start.block, start.row) {
            start.column
        } else {
            0
        };
        let last = if row == (end.block, end.row) {
            end.column.saturating_add(1)
        } else {
            usize::MAX
        };
        Some(first..last)
    }

    pub fn paint(&self, line: &Line, point: Point) -> Line {
        let Some(selected) = self.columns(point) else {
            return line.clone();
        };
        let mut column = 0;
        let mut spans: Vec<Span> = Vec::new();
        for span in &line.spans {
            for text in span.text.graphemes(true) {
                let next = column + util::width(text);
                let mut style = span.style;
                if next > selected.start && column < selected.end {
                    style.bg = Bg::Selected;
                    style.fg = Color::Text;
                }
                if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                    last.text.push_str(text);
                } else {
                    spans.push(Span::new(text, style));
                }
                column = next;
            }
        }
        Line::spans(spans)
    }

    pub fn append(&self, out: &mut String, row: &HistoryRow<Line>, point: Point) {
        let Some(selected) = self.columns(point) else {
            return;
        };
        let text = row.line.text();
        let mut column = 0;
        for text in text.graphemes(true) {
            let next = column + util::width(text);
            if next > selected.start && column < selected.end {
                out.push_str(text);
            }
            column = next;
        }
        let end = self.anchor.max(self.focus);
        if !row.wrapped && (point.block, point.row) < (end.block, end.row) {
            out.push('\n');
        }
    }
}

impl Screen {
    pub(super) fn copy_selection(&mut self) {
        use base64::Engine;
        let text = if self.detail.is_some() {
            self.detail_selection()
        } else {
            self.transcript_selection()
        };
        if !text.is_empty() {
            let data = base64::engine::general_purpose::STANDARD.encode(text);
            // OSC 52 addresses the user's terminal clipboard, including over SSH.
            let _ = write!(self.out, "\x1b]52;c;{data}\x07");
            let _ = self.out.flush();
        }
    }

    pub(super) fn copy_menu_event(&mut self, event: &Event) -> bool {
        if let Event::Mouse(mouse) = event
            && mouse.kind == event::MouseEventKind::Down(event::MouseButton::Right)
        {
            let selected = if let Some(detail) = &self.detail {
                detail.has_selection()
            } else {
                self.transcript_view
                    .as_ref()
                    .is_some_and(|view| view.has_selection())
            };
            if selected {
                if let Some(view) = self.transcript_view.as_mut() {
                    view.rendered = None;
                }
                self.copy_menu = Some((
                    usize::from(mouse.row).min(self.height.saturating_sub(2)),
                    usize::from(mouse.column).min(self.width.saturating_sub(6)),
                ));
                return true;
            }
        }
        let Some((row, column)) = self.copy_menu else {
            return false;
        };
        if matches!(event, Event::Resize(..)) {
            self.copy_menu = None;
            if let Some(view) = self.transcript_view.as_mut() {
                view.rendered = None;
            }
            return false;
        }
        match event {
            Event::Mouse(mouse)
                if mouse.kind == event::MouseEventKind::Down(event::MouseButton::Left) =>
            {
                if usize::from(mouse.row) == row
                    && (column..column + 6).contains(&usize::from(mouse.column))
                {
                    self.copy_selection();
                }
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    event::MouseEventKind::Up(_) | event::MouseEventKind::Moved
                ) =>
            {
                return true;
            }
            Event::Key(key) if key.code == KeyCode::Enter => self.copy_selection(),
            _ => {}
        }
        self.copy_menu = None;
        if let Some(view) = self.transcript_view.as_mut() {
            view.rendered = None;
        }
        true
    }

    pub(super) fn draw_copy_menu(&mut self) {
        if let Some((row, column)) = self.copy_menu {
            let _ = write!(self.out, "\x1b[?2026h");
            for (offset, label) in [" 复制 ", " 取消 "]
                .iter()
                .enumerate()
                .take(self.height - row)
            {
                let line = Line::new(*label, Style::with_bg(Color::Text, Bg::Selected))
                    .truncate(self.width.saturating_sub(column));
                let _ = write!(
                    self.out,
                    "\x1b[{};{}H{}",
                    row + offset + 1,
                    column + 1,
                    self.paint(&line)
                );
            }
            let _ = write!(self.out, "\x1b[?25l\x1b[?2026l");
            let _ = self.out.flush();
        }
    }
}
