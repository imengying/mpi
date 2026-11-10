//! Incremental painting inside the session's alternate screen.

use crate::ui::text::HistoryRow;
use std::fmt::Write;

#[derive(Default)]
pub(super) struct Viewport {
    size: Option<(usize, usize)>,
    top: usize,
    rows: Vec<HistoryRow<String>>,
    cursor: Option<(usize, usize)>,
    invalidated: bool,
}

impl Viewport {
    pub(super) fn needs_reflow(&self, width: usize, height: usize, live_rows: usize) -> bool {
        self.size.is_some_and(|size| {
            size != (width, height) || self.invalidated || live_rows < self.rows.len()
        })
    }

    /// Build one synchronized frame. Reflow restores history after a resize or menu shrink.
    pub(super) fn draw(
        &mut self,
        size: (usize, usize),
        rows: Vec<HistoryRow<String>>,
        cursor: Option<(usize, usize)>,
        history: &[HistoryRow<String>],
        reflow: &[HistoryRow<String>],
    ) -> String {
        let (width, height) = size;
        debug_assert!(width > 0 && height > 0 && !rows.is_empty() && rows.len() <= height);
        let first_frame = self.size.is_none();
        let reflowing = self.needs_reflow(width, height, rows.len());
        let layout_changed = first_frame || reflowing || self.rows.len() != rows.len();
        let mut frame = String::from("\x1b[?2026h");

        if reflowing {
            let shown = reflow.len().min(height - rows.len());
            erase_rows(&mut frame, 0, height);
            write_rows(&mut frame, 0, &reflow[reflow.len() - shown..]);
            self.top = shown;
        } else if layout_changed || !history.is_empty() {
            // The previous live rows are ours. Clear them before appending history or growing
            // the composer, so drafts can never be moved into native scrollback.
            erase_rows(&mut frame, self.top, self.rows.len());
        }

        if !history.is_empty() {
            write_rows(&mut frame, self.top, history);
            // Reserve the live rows, retaining native wrap flags for text selection.
            frame.push_str(&"\r\n\x1b[K".repeat(rows.len()));
            self.top = (self.top + history.len()).min(height - rows.len());
        } else {
            let scroll = (self.top + rows.len()).saturating_sub(height);
            if scroll > 0 {
                // Old live rows were erased before scrolling, so drafts never enter history.
                scroll_screen(&mut frame, height, scroll);
                self.top -= scroll;
            }
        }
        let top = self.top;
        let repaint = layout_changed || !history.is_empty();
        let mut index = 0;
        while index < rows.len() {
            let start = index;
            while index + 1 < rows.len() && rows[index].wrapped {
                index += 1;
            }
            let end = index + 1;
            if repaint || self.rows.get(start..end) != Some(&rows[start..end]) {
                // Clear the whole logical line first, then write it without cursor moves
                // at soft boundaries. Cursor-addressed repainting loses native wrap flags.
                for row in start..end {
                    move_to(&mut frame, top + row, 0);
                    frame.push_str("\x1b[K");
                }
                move_to(&mut frame, top + start, 0);
                for row in &rows[start..end] {
                    frame.push_str(&row.line);
                }
            }
            index = end;
        }
        if cursor.is_some() != self.cursor.is_some() || self.invalidated || self.size.is_none() {
            frame.push_str(if cursor.is_some() {
                "\x1b[?25h"
            } else {
                "\x1b[?25l"
            });
        }
        if let Some((row, column)) = cursor {
            move_to(&mut frame, top + row, column.min(width.saturating_sub(1)));
        } else {
            move_to(&mut frame, top + rows.len() - 1, 0);
        }
        frame.push_str("\x1b[?2026l");
        self.size = Some(size);
        self.rows = rows;
        self.cursor = cursor;
        self.invalidated = false;
        frame
    }

    pub(super) fn erase(&mut self) -> String {
        if self.size.is_none() || self.rows.is_empty() {
            return String::new();
        }
        let mut frame = String::from("\x1b[?2026h");
        erase_rows(&mut frame, self.top, self.rows.len());
        // Return to the transcript boundary before a modal overlay or redraw.
        move_to(&mut frame, self.top, 0);
        frame.push_str("\x1b[?25h\x1b[?2026l");
        self.rows.clear();
        self.cursor = None;
        self.invalidated = true;
        frame
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

fn erase_rows(frame: &mut String, top: usize, count: usize) {
    for row in top..top.saturating_add(count) {
        move_to(frame, row, 0);
        frame.push_str("\x1b[K");
    }
}

fn write_rows(frame: &mut String, top: usize, rows: &[HistoryRow<String>]) {
    if rows.is_empty() {
        return;
    }
    move_to(frame, top, 0);
    frame.push_str("\x1b[K");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 && !rows[index - 1].wrapped {
            frame.push_str("\r\n\x1b[K");
        }
        frame.push_str(&row.line);
    }
}

fn scroll_screen(frame: &mut String, height: usize, count: usize) {
    move_to(frame, height.saturating_sub(1), 0);
    frame.push_str(&"\r\n".repeat(count));
}

fn move_to(frame: &mut String, row: usize, column: usize) {
    let _ = write!(frame, "\x1b[{};{}H", row + 1, column + 1);
}

#[cfg(test)]
mod tests {
    /// vt100 0.18 takes `NonZeroU16`: a zero size panicked before, so the type now says so.
    fn nz(value: u16) -> std::num::NonZeroU16 {
        std::num::NonZeroU16::new(value).expect("test dimensions are never zero")
    }

    use super::*;

    fn rows(text: &[&str]) -> Vec<HistoryRow<String>> {
        text.iter()
            .map(|text| HistoryRow::hard((*text).to_owned()))
            .collect()
    }

    fn history(text: &[&str]) -> Vec<HistoryRow<String>> {
        rows(text)
    }

    #[test]
    fn history_reaches_the_bottom_without_drafts_in_scrollback() {
        let mut terminal = vt100::Parser::new(nz(12), nz(24), 200);
        let mut view = Viewport::default();
        let base = rows(&["working", "draft", "directory", "stats"]);
        for (count, caret_row) in [(0, 1), (1, 2), (40, 9)] {
            let history = (0..count)
                .map(|index| HistoryRow {
                    line: format!("output {index}"),
                    wrapped: false,
                })
                .collect::<Vec<_>>();
            terminal.process(
                view.draw((24, 12), base.clone(), Some((1, 5)), &history, &[])
                    .as_bytes(),
            );
            let visible = terminal.screen().contents();
            assert!(
                visible.ends_with("working\ndraft\ndirectory\nstats"),
                "{visible}"
            );
            assert_eq!(terminal.screen().cursor_position(), (caret_row, 5));
        }
        terminal.screen_mut().set_scrollback(200);
        let all = terminal.screen().contents();
        assert!(all.contains("output 0"), "{all}");
        assert!(
            !all.contains("draft"),
            "composer leaked into scrollback: {all}"
        );
    }

    #[test]
    fn a_shrinking_live_area_stays_docked_after_the_screen_fills() {
        let mut terminal = vt100::Parser::new(nz(12), nz(24), 0);
        let mut view = Viewport::default();
        let transcript: Vec<_> = (0..30)
            .map(|i| HistoryRow::hard(format!("answer {i}")))
            .collect();
        terminal.process(
            view.draw(
                (24, 12),
                rows(&["one", "two", "three", "draft", "stats"]),
                Some((3, 5)),
                &transcript,
                &[],
            )
            .as_bytes(),
        );
        terminal.process(
            view.draw(
                (24, 12),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &[],
                &transcript,
            )
            .as_bytes(),
        );
        assert_eq!(terminal.screen().rows(0, 24).last().unwrap(), "stats");
        assert_eq!(terminal.screen().cursor_position(), (10, 5));
        assert!(terminal.screen().contents().contains("answer 29"));
        assert_eq!(terminal.screen().rows(0, 24).nth(9).unwrap(), "answer 29");
        assert!(!terminal.screen().contents().contains("three"));
    }

    #[test]
    fn reflow_preserves_visible_history_and_inserts_fresh_output_once() {
        let mut terminal = vt100::Parser::new(nz(12), nz(24), 100);
        let mut view = Viewport::default();
        terminal.process(
            view.draw(
                (24, 12),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &history(&["answer"]),
                &[],
            )
            .as_bytes(),
        );
        for width in [16, 30] {
            terminal.screen_mut().set_size(nz(12), nz(width));
            terminal.process(
                view.draw(
                    (width.into(), 12),
                    rows(&["draft", "stats"]),
                    Some((0, 5)),
                    &[],
                    &history(&["answer"]),
                )
                .as_bytes(),
            );
            assert_eq!(terminal.screen().contents(), "answer\ndraft\nstats");
        }
        terminal.process(view.erase().as_bytes());
        terminal.process(
            view.draw(
                (30, 12),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &history(&["exit hint"]),
                &history(&["answer"]),
            )
            .as_bytes(),
        );
        terminal.process(view.erase().as_bytes());
        assert_eq!(terminal.screen().contents(), "answer\nexit hint");
        assert_eq!(terminal.screen().cursor_position(), (2, 0));
        terminal.screen_mut().set_scrollback(100);
        assert_eq!(
            terminal.screen().scrollback(),
            0,
            "no unnecessary scrolling on reflow"
        );
    }

    #[test]
    fn growing_and_shrinking_input_preserves_the_visible_transcript() {
        let mut terminal = vt100::Parser::new(nz(10), nz(20), 100);
        let mut view = Viewport::default();
        for live in [
            rows(&["draft", "stats"]),
            rows(&["first", "second", "third", "stats"]),
            rows(&["draft", "stats"]),
        ] {
            let history = if view.size.is_none() {
                history(&["answer"])
            } else {
                Vec::new()
            };
            terminal.process(
                view.draw(
                    (20, 10),
                    live.clone(),
                    Some((0, 0)),
                    &history,
                    &rows(&["answer"]),
                )
                .as_bytes(),
            );
            let visible = terminal.screen().contents();
            assert!(visible.contains("answer"), "{visible}");
            assert!(
                visible.ends_with(
                    &live
                        .iter()
                        .map(|row| row.line.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                "{visible}"
            );
        }
    }

    #[test]
    fn spinner_ticks_do_not_rewrite_the_editor_or_status() {
        let mut view = Viewport::default();
        let base = rows(&["thinking", "working", "draft", "stats"]);
        view.draw((20, 10), base, Some((2, 5)), &[], &[]);
        let frame = view.draw(
            (20, 10),
            rows(&["new thought", "spinner", "draft", "stats"]),
            Some((2, 5)),
            &[],
            &[],
        );
        assert!(!frame.contains("draft"));
        assert!(!frame.contains("stats"));
        assert!(!frame.contains("\x1b[J"));
    }

    #[test]
    fn native_selection_keeps_a_hard_boundary_across_a_collapsed_gap() {
        use crate::ui::text::{Block, Line};
        let source = "0123456789abcdefghijABCDEFGHIJ";
        let mut block = Block::collapsible_excerpted(vec![Line::plain(source)], 0, 0, 1, 1);
        for expanded in [false, true] {
            if let Block::Collapsible(inner) = &mut block {
                inner.expanded = expanded;
            }
            let history = block
                .history_rows(10)
                .into_iter()
                .map(|row| HistoryRow {
                    line: row.line.text(),
                    wrapped: row.wrapped,
                })
                .collect::<Vec<_>>();
            let mut terminal = vt100::Parser::new(nz(12), nz(10), 100);
            let mut view = Viewport::default();
            terminal.process(
                view.draw(
                    (10, 12),
                    rows(&["draft", "stats"]),
                    Some((0, 0)),
                    &history,
                    &[],
                )
                .as_bytes(),
            );
            assert_eq!(
                terminal
                    .screen()
                    .contents_between(0, 0, history.len() as u16 - 1, 10),
                if expanded {
                    source
                } else {
                    "0123456789\nABCDEFGHIJ"
                }
            );
        }
    }

    #[test]
    fn resize_reflows_visible_history_and_reanchors_the_cursor() {
        let mut terminal = vt100::Parser::new(nz(12), nz(24), 100);
        let mut view = Viewport::default();
        terminal.process(
            view.draw(
                (24, 12),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &history(&["history", "tail"]),
                &[],
            )
            .as_bytes(),
        );
        for (width, height, caret_row) in [(16, 8, 2), (30, 16, 2), (8, 3, 1)] {
            terminal
                .screen_mut()
                .set_size(nz(height as u16), nz(width as u16));
            let frame = view.draw(
                (width, height),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &[],
                &history(&["history", "tail"]),
            );
            terminal.process(frame.as_bytes());
            assert!(terminal.screen().contents().ends_with("tail\ndraft\nstats"));
            assert_eq!(terminal.screen().cursor_position(), (caret_row, 5));
        }
    }
}
