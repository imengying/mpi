//! A bottom-anchored inline viewport; finalized output stays in native scrollback.

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
    pub(super) fn needs_reflow(&self, width: usize, height: usize) -> bool {
        self.size
            .is_some_and(|size| size != (width, height) || self.invalidated)
    }

    /// Build one synchronized frame. `reflow` is the visible history tail after a resize.
    pub(super) fn draw(
        &mut self,
        size: (usize, usize),
        rows: Vec<HistoryRow<String>>,
        cursor: Option<(usize, usize)>,
        history: &[HistoryRow<String>],
        reflow: &[HistoryRow<String>],
    ) -> String {
        let (width, height) = size;
        let top = height.saturating_sub(rows.len());
        let resized = self.needs_reflow(width, height);
        let moved = self.top != top || resized || self.size.is_none();
        let mut frame = String::from("\x1b[?2026h");

        if self.size.is_none() {
            // Move the shell's existing screen into scrollback before owning the bottom.
            frame.push_str(&"\r\n".repeat(height));
        } else if resized {
            // Rebuild visible history at the new width without replaying it into scrollback.
            frame.push_str("\x1b[H\x1b[J");
            let start = reflow.len().saturating_sub(top);
            let padding = top.saturating_sub(reflow.len());
            for (index, row) in reflow[start..].iter().enumerate() {
                if index == 0 || !reflow[start + index - 1].wrapped {
                    move_to(&mut frame, padding + index, 0);
                }
                frame.push_str(&row.line);
            }
        } else if moved {
            move_to(&mut frame, self.top, 0);
            frame.push_str("\x1b[J");
            if top < self.top {
                // Full-screen newlines preserve history in terminals that discard partial
                // scroll regions. The erased composer cannot leak into scrollback.
                move_to(&mut frame, height - 1, 0);
                frame.push_str(&"\r\n".repeat(self.top - top));
            } else {
                // Only blank composer rows leave the bottom; visible history moves down.
                let _ = write!(frame, "\x1b[{}T", top - self.top);
            }
        }

        if !history.is_empty() {
            move_to(&mut frame, top, 0);
            frame.push_str("\x1b[J");
            for (index, row) in history.iter().enumerate() {
                if index > 0 && !history[index - 1].wrapped {
                    frame.push_str("\r\n");
                }
                frame.push_str(&row.line);
            }
            // Advance through blank rows, leaving the last history row above the viewport.
            frame.push_str(&"\r\n\x1b[K".repeat(rows.len().max(1)));
        }

        let repaint = moved || self.invalidated || !history.is_empty();
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
            move_to(&mut frame, height - 1, 0);
        }
        frame.push_str("\x1b[?2026l");
        self.size = Some(size);
        self.top = top;
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
        move_to(&mut frame, self.top, 0);
        frame.push_str("\x1b[J\x1b[?2026l");
        self.rows.clear();
        self.cursor = None;
        self.invalidated = true;
        frame
    }

    pub(super) fn clear(&mut self) {
        self.rows.clear();
        self.cursor = None;
        self.invalidated = true;
    }
}

fn move_to(frame: &mut String, row: usize, column: usize) {
    let _ = write!(frame, "\x1b[{};{}H", row + 1, column + 1);
}

#[cfg(test)]
mod tests {
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
    fn history_and_live_output_keep_the_composer_at_the_bottom() {
        let mut terminal = vt100::Parser::new(12, 24, 200);
        terminal.process(b"shell output\r\n");
        let mut view = Viewport::default();
        let base = rows(&["working", "draft", "directory", "stats"]);
        for count in [0, 1, 40] {
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
            assert_eq!(terminal.screen().cursor_position(), (9, 5));
        }
        terminal.screen_mut().set_scrollback(200);
        let all = terminal.screen().contents();
        assert!(all.contains("shell output"), "{all}");
        assert!(
            !all.contains("draft"),
            "composer leaked into scrollback: {all}"
        );
    }

    #[test]
    fn growing_and_shrinking_input_preserves_the_visible_transcript() {
        let mut terminal = vt100::Parser::new(10, 20, 100);
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
                view.draw((20, 10), live.clone(), Some((0, 0)), &history, &[])
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
        let mut block = Block::collapsible(vec![Line::plain(source)], 1, 0, 1);
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
            let mut terminal = vt100::Parser::new(12, 10, 100);
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
                    .contents_between(10 - history.len() as u16, 0, 9, 10),
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
        let mut terminal = vt100::Parser::new(12, 24, 100);
        let mut view = Viewport::default();
        view.draw((24, 12), rows(&["draft", "stats"]), Some((0, 5)), &[], &[]);
        for (width, height) in [(16, 8), (30, 16), (8, 3)] {
            terminal.screen_mut().set_size(height as u16, width as u16);
            let frame = view.draw(
                (width, height),
                rows(&["draft", "stats"]),
                Some((0, 5)),
                &[],
                &history(&["history", "tail"]),
            );
            terminal.process(frame.as_bytes());
            assert!(terminal.screen().contents().ends_with("tail\ndraft\nstats"));
            assert_eq!(terminal.screen().cursor_position(), (height as u16 - 2, 5));
        }
    }
}
