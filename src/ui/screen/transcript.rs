//! Mouse browsing and selection, anchored to immutable blocks rather than a flat history.
use super::*;
use selection::{Point, Selection};
use std::sync::Arc;

pub(super) struct TranscriptView {
    end: usize,
    top: Point,
    tail: Arc<Vec<HistoryRow<Line>>>,
    visible: Vec<(Point, HistoryRow<Line>)>,
    selection: Option<Selection>,
    at_bottom: bool,
    pub(super) rendered: Option<(usize, usize)>,
}

impl TranscriptView {
    pub(super) fn has_selection(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    fn rows(&self, screen: &mut Screen, block: usize) -> Arc<Vec<HistoryRow<Line>>> {
        if block == self.end {
            self.tail.clone()
        } else {
            screen
                .layout
                .rows(block, screen.width, &screen.blocks[block])
        }
    }

    fn move_by(&mut self, screen: &mut Screen, movement: isize) {
        let mut remaining = movement.unsigned_abs();
        while remaining > 0 {
            if movement > 0 {
                let moved = remaining.min(self.top.row);
                remaining -= moved;
                self.top.row -= moved;
                if remaining == 0 || self.top.block == 0 {
                    break;
                }
                self.top.block -= 1;
                self.top.row = self.rows(screen, self.top.block).len();
            } else if self.top.block <= self.end {
                let rows = self.rows(screen, self.top.block);
                let moved = remaining.min(rows.len().saturating_sub(self.top.row));
                remaining -= moved;
                self.top.row += moved;
                if self.top.row >= rows.len() {
                    self.top.block += 1;
                    self.top.row = 0;
                }
            } else {
                break;
            }
        }
        self.rendered = None;
        self.compose(screen);
    }

    fn compose(&mut self, screen: &mut Screen) {
        self.visible.clear();
        let mut point = self.top;
        while point.block <= self.end && self.visible.len() < screen.height {
            let rows = self.rows(screen, point.block);
            for (row, line) in rows.iter().enumerate().skip(point.row) {
                self.visible.push((
                    Point {
                        block: point.block,
                        row,
                        column: 0,
                    },
                    line.clone(),
                ));
                if self.visible.len() == screen.height {
                    break;
                }
            }
            let next_row = self
                .visible
                .last()
                .filter(|(last, _)| last.block == point.block)
                .map_or(point.row, |(last, _)| last.row + 1);
            if next_row < rows.len() {
                break;
            }
            point.block += 1;
            point.row = 0;
        }
        self.at_bottom = point.block > self.end;
    }

    fn point(&self, row: usize, column: usize) -> Option<Point> {
        self.visible
            .get(row.min(self.visible.len().saturating_sub(1)))
            .map(|(point, _)| Point { column, ..*point })
    }
}

impl Screen {
    fn open_transcript(&mut self) -> bool {
        if self.transcript_view.is_some() {
            return true;
        }
        if self.printed == 0 && self.live_preview.is_empty() {
            return false;
        }
        let mut view = TranscriptView {
            end: self.printed,
            top: Point {
                block: self.printed,
                ..Point::default()
            },
            tail: Arc::new(self.live_preview.clone()),
            visible: Vec::new(),
            selection: None,
            at_bottom: true,
            rendered: None,
        };
        view.move_by(self, self.viewport.history_height() as isize);
        self.transcript_view = Some(view);
        true
    }

    pub(super) fn transcript_event(&mut self, event: &Event) -> bool {
        if let Event::Mouse(mouse) = event {
            use event::{MouseButton::Left, MouseEventKind::*};
            match mouse.kind {
                ScrollUp | ScrollDown => {
                    if mouse.kind == ScrollDown && self.transcript_view.is_none() {
                        return true;
                    }
                    if self.open_transcript() {
                        self.move_transcript(if mouse.kind == ScrollUp { 3 } else { -3 });
                    }
                }
                Down(Left) => {
                    if let Some(view) = self.transcript_view.as_mut() {
                        if let Some(point) =
                            view.point(usize::from(mouse.row), usize::from(mouse.column))
                        {
                            view.selection = Some(Selection::new(point, *mouse));
                            view.rendered = None;
                        }
                    } else if usize::from(mouse.row)
                        < self.viewport.history_height() + self.live_preview.len()
                    {
                        self.drag_start = Some(*mouse);
                    }
                }
                Drag(Left) | Up(Left) => {
                    if mouse.kind == Drag(Left)
                        && let Some(start) = self.drag_start.take()
                        && self.open_transcript()
                    {
                        let view = self.transcript_view.as_mut().unwrap();
                        if let Some(point) =
                            view.point(usize::from(start.row), usize::from(start.column))
                        {
                            view.selection = Some(Selection::new(point, start));
                        }
                    }
                    if let Some(view) = self.transcript_view.as_mut()
                        && let Some(point) =
                            view.point(usize::from(mouse.row), usize::from(mouse.column))
                        && let Some(selection) = view
                            .selection
                            .as_mut()
                            .filter(|selection| selection.dragging)
                    {
                        selection.update(point, *mouse);
                        view.rendered = None;
                    }
                    if mouse.kind == Up(Left) {
                        self.drag_start = None;
                    }
                }
                _ => {}
            }
            return true;
        }
        self.drag_start = None;
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return false;
            }
            if self.transcript_view.is_some() {
                match key.code {
                    KeyCode::Up => self.move_transcript(1),
                    KeyCode::Down => self.move_transcript(-1),
                    KeyCode::Home => {
                        let view = self.transcript_view.as_mut().unwrap();
                        view.top = Point::default();
                        view.rendered = None;
                    }
                    KeyCode::End | KeyCode::Esc => self.close_transcript(),
                    KeyCode::Char('c')
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && self.transcript_view.as_ref().unwrap().has_selection() =>
                    {
                        self.copy_selection()
                    }
                    _ => {
                        self.close_transcript();
                        return false;
                    }
                }
                return true;
            }
        }
        // Typing and resizing return to the live composer; a resize cancels cell anchors.
        self.close_transcript();
        false
    }

    fn move_transcript(&mut self, movement: isize) {
        let Some(mut view) = self.transcript_view.take() else {
            return;
        };
        if movement < 0
            && view.at_bottom
            && !view
                .selection
                .as_ref()
                .is_some_and(|selection| selection.dragging)
        {
            self.erase_live();
            self.copy_menu = None;
            return;
        }
        if movement > 0 || !view.at_bottom {
            view.move_by(self, movement);
        }
        if let Some((row, column)) = view
            .selection
            .as_ref()
            .and_then(Selection::dragging_pointer)
            && let Some(point) = view.point(row, column)
        {
            view.selection.as_mut().unwrap().extend_to(point);
        }
        self.transcript_view = Some(view);
    }

    pub(super) fn tick_transcript_selection(&mut self, now: std::time::Instant) -> bool {
        let Some(mut view) = self.transcript_view.take() else {
            return false;
        };
        let movement = view
            .selection
            .as_mut()
            .and_then(|selection| selection.scroll(now, 0, self.height.saturating_sub(1)));
        let mut changed = false;
        if let Some((delta, row, column)) = movement {
            if delta > 0 || !view.at_bottom {
                view.move_by(self, delta);
            }
            if let Some(point) = view.point(row, column) {
                view.selection.as_mut().unwrap().extend_to(point);
                view.rendered = None;
                changed = true;
            }
        }
        self.transcript_view = Some(view);
        changed
    }

    pub(super) fn transcript_selection(&mut self) -> String {
        let Some(view) = self.transcript_view.take() else {
            return String::new();
        };
        let mut text = String::new();
        if let Some(selection) = &view.selection {
            for block in selection.blocks() {
                for (row, line) in view.rows(self, block).iter().enumerate() {
                    selection.append(
                        &mut text,
                        line,
                        Point {
                            block,
                            row,
                            column: 0,
                        },
                    );
                }
            }
        }
        self.transcript_view = Some(view);
        text
    }

    pub(super) fn close_transcript(&mut self) {
        self.drag_start = None;
        self.copy_menu = None;
        if self.transcript_view.take().is_some() {
            self.erase_live();
        }
    }

    pub(super) fn draw_transcript(&mut self) {
        let Some(mut view) = self.transcript_view.take() else {
            return;
        };
        if view.rendered != Some((self.width, self.height)) {
            view.compose(self);
            let mut rows: Vec<_> = view
                .visible
                .iter()
                .map(|(point, row)| {
                    let line = view.selection.as_ref().map_or_else(
                        || row.line.clone(),
                        |selection| selection.paint(&row.line, *point),
                    );
                    HistoryRow {
                        line: self.paint(&line),
                        wrapped: row.wrapped,
                    }
                })
                .collect();
            if let Some(last) = rows.last_mut() {
                last.wrapped = false;
            }
            rows.resize_with(self.height, || HistoryRow::hard(String::new()));
            let frame = Viewport::default().draw((self.width, self.height), rows, None, &[], &[]);
            let _ = write!(self.out, "{frame}");
            let _ = self.out.flush();
            view.rendered = Some((self.width, self.height));
        }
        self.transcript_view = Some(view);
    }
}
