//! Browse completed output without putting it in the shell's scrollback.

use super::*;

pub(super) struct TranscriptView {
    /// Freeze the last block while browsing so new output cannot move the page.
    end: usize,
    block: usize,
    row: usize,
    at_bottom: bool,
    pub(super) rendered: Option<(usize, usize)>,
}

impl Screen {
    pub(super) fn transcript_event(&mut self, event: &Event) -> bool {
        let page = self.height.saturating_sub(2).max(1);
        let movement = match event {
            Event::Mouse(mouse) => match mouse.kind {
                event::MouseEventKind::ScrollUp => Some(3isize),
                event::MouseEventKind::ScrollDown => Some(-3),
                _ => return false,
            },
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::PageUp => Some(page as isize),
                KeyCode::PageDown => Some(-(page as isize)),
                KeyCode::Up if self.transcript_view.is_some() => Some(1),
                KeyCode::Down if self.transcript_view.is_some() => Some(-1),
                KeyCode::Home if self.transcript_view.is_some() => {
                    let view = self.transcript_view.as_mut().unwrap();
                    view.block = 0;
                    view.row = 0;
                    view.at_bottom = false;
                    view.rendered = None;
                    return true;
                }
                KeyCode::End | KeyCode::Esc if self.transcript_view.is_some() => {
                    self.close_transcript();
                    return true;
                }
                _ => None,
            },
            Event::Paste(_) => None,
            _ => return false,
        };
        if let Some(movement) = movement {
            self.move_transcript(movement);
            return true;
        }
        // Typing resumes the composer without swallowing the first keystroke or paste.
        self.close_transcript();
        false
    }

    fn move_transcript(&mut self, movement: isize) {
        let opening = self.transcript_view.is_none();
        if opening && (movement < 0 || self.blocks.is_empty()) {
            return;
        }
        if movement < 0
            && self
                .transcript_view
                .as_ref()
                .is_some_and(|view| view.at_bottom)
        {
            self.close_transcript();
            return;
        }
        let view = self.transcript_view.get_or_insert(TranscriptView {
            end: self.blocks.len(),
            block: self.blocks.len(),
            row: 0,
            at_bottom: false,
            rendered: None,
        });
        let mut remaining = movement.unsigned_abs();
        if opening {
            remaining += self.height.saturating_sub(1).max(1);
        }
        // Keep a block/row anchor: distant pages never rerender all the newer blocks.
        while remaining > 0 {
            if movement > 0 {
                let moved = remaining.min(view.row);
                remaining -= moved;
                view.row -= moved;
                if remaining == 0 || view.block == 0 {
                    break;
                }
                view.block -= 1;
                view.row = self
                    .layout
                    .rows(view.block, self.width, &self.blocks[view.block])
                    .len();
            } else if view.block < view.end {
                let rows = self
                    .layout
                    .rows(view.block, self.width, &self.blocks[view.block]);
                let moved = remaining.min(rows.len().saturating_sub(view.row));
                remaining -= moved;
                view.row += moved;
                if view.row >= rows.len() {
                    view.block += 1;
                    view.row = 0;
                }
            } else {
                break;
            }
        }
        view.rendered = None;
        if movement > 0 {
            view.at_bottom = false;
        }
        if view.block == view.end {
            self.close_transcript();
        }
    }

    pub(super) fn close_transcript(&mut self) {
        if self.transcript_view.take().is_some() {
            self.erase_live();
        }
    }

    pub(super) fn draw_transcript(&mut self) {
        let view = self.transcript_view.as_mut().expect("open transcript");
        if view.rendered == Some((self.width, self.height)) {
            return;
        }
        let page = self.height.saturating_sub(1).max(1);
        let mut rows = Vec::new();
        let mut block = view.block;
        let mut row = view.row;
        while block < view.end {
            let current = self.layout.rows(block, self.width, &self.blocks[block]);
            row = row.min(current.len());
            let end = (row + page - rows.len()).min(current.len());
            rows.extend_from_slice(&current[row..end]);
            if end == current.len() {
                block += 1;
                row = 0;
            }
            if rows.len() == page {
                break;
            }
        }
        view.at_bottom = block == view.end;
        view.rendered = Some((self.width, self.height));
        if let Some(last) = rows.last_mut() {
            last.wrapped = false;
        }
        if self.height > 1 {
            rows.resize_with(page, || HistoryRow::hard(Line::blank()));
            rows.push(HistoryRow::hard(
                Line::dim("滚轮 / PgUp/PgDn 翻页 · End / Esc 返回").truncate(self.width),
            ));
        }
        if rows.is_empty() {
            rows.push(HistoryRow::hard(Line::blank()));
        }
        let painted = rows
            .iter()
            .map(|row| HistoryRow {
                line: self.paint(&row.line),
                wrapped: row.wrapped,
            })
            .collect();
        let mut viewport = Viewport::default();
        let frame = viewport.draw((self.width, self.height), painted, None, &[], &[]);
        let _ = write!(self.out, "{frame}");
        let _ = self.out.flush();
    }
}
