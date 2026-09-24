//! Fenced-code layout and table layout.
//!
//! A fenced block is drawn as a frame around its text and nothing more. The text is passed
//! through exactly as the model wrote it — no tokeniser, no colour — so what a reader copies
//! out of a block is what the model produced, and a language this code has never heard of is
//! no worse off than one it has.

use crate::ui::text::{Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

use super::inline;

/// How wide a table may get before the border is dropped.
///
/// A bordered table that has to wrap is worse than a plain one: the columns stop lining up
/// exactly when the alignment was the reason for the border. Past this the rows are laid
/// out as plain text with the pipes removed.
const TABLE_MAX_WIDTH: usize = 100;

/// Flush the pending run into `spans`, if there is one.
pub(super) fn push(spans: &mut Vec<Span>, buf: &mut String, style: Style) {
    if buf.is_empty() {
        return;
    }
    spans.push(Span::new(std::mem::take(buf), style));
}

/// Whether `mark` appears at `at`.
pub(super) fn starts_with(chars: &[char], at: usize, mark: &[char]) -> bool {
    chars.get(at..at + mark.len()).is_some_and(|got| got == mark)
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
    Center,
}

pub(super) fn render_table(rows: &[String], width: usize) -> Vec<Line> {
    // The second row has to be the separator, or these lines are not a table at all —
    // which is what keeps a half-streamed table from being eaten as one.
    let Some(header_align) = rows.get(1).and_then(|row| parse_separator(row)) else {
        return rows.iter().map(|row| Line::spans(inline(row.trim_end(), Style::plain()))).collect();
    };
    let header = split_row(&rows[0]);
    let mut aligns = header_align;
    let mut body: Vec<Vec<String>> = Vec::new();
    for row in &rows[2..] {
        let cells = split_row(row);
        // A row with more cells than the header adds columns the separator said nothing
        // about; they are left aligned.
        aligns.resize(aligns.len().max(cells.len()), Align::Left);
        body.push(cells);
    }
    let cols = header.len().max(aligns.len());
    if cols == 0 {
        return Vec::new();
    }
    let mut table: Vec<Vec<String>> = vec![header];
    table.extend(body);

    // Natural width: the widest cell per column, header included.
    let mut widths = vec![0usize; cols];
    for row in &table {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(util::width(cell.trim()));
        }
    }
    // "│ " + cells joined by " │ " + " │" is 3 columns per cell plus 1.
    let overhead = 3 * cols + 1;
    let fits = width == 0 || widths.iter().sum::<usize>() + overhead <= width;
    if width > 0 && (width > TABLE_MAX_WIDTH || !fits) {
        // Too wide for a table to be a table: plain rows, pipes removed, so nothing
        // pretends to be a grid.
        return table
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let text = row.iter().map(|cell| cell.trim()).filter(|cell| !cell.is_empty()).collect::<Vec<_>>().join("  ");
                let style = if index == 0 { Style::bold(Color::Text) } else { Style::plain() };
                Line::spans(inline(&text, style))
            })
            .collect();
    }

    let border = |left: &str, mid: &str, right: &str| {
        let mut text = String::from(left);
        for (index, w) in widths.iter().enumerate() {
            text.push_str(&"─".repeat(w + 2));
            text.push_str(if index + 1 == widths.len() { right } else { mid });
        }
        Line::new(text, Style::new(Color::Dim))
    };
    let mut out = vec![border("┌", "┬", "┐")];
    for (row_index, row) in table.iter().enumerate() {
        let header_row = row_index == 0;
        let mut spans: Vec<Span> = vec![Span::new("│", Style::new(Color::Dim))];
        for (index, w) in widths.iter().enumerate() {
            let cell = row.get(index).map(|c| c.trim()).unwrap_or("");
            let pad = w.saturating_sub(util::width(cell));
            let (left, right) = match aligns.get(index).copied().unwrap_or(Align::Left) {
                Align::Left => (0, pad),
                Align::Right => (pad, 0),
                Align::Center => (pad / 2, pad - pad / 2),
            };
            spans.push(Span::plain(" ".repeat(left + 1)));
            // Cells are inline markdown like any other text.
            let style = if header_row { Style::bold(Color::Text) } else { Style::plain() };
            let mut cell_spans = inline(cell, style);
            if header_row {
                for span in &mut cell_spans {
                    span.style.bold = true;
                }
            }
            spans.extend(cell_spans);
            spans.push(Span::plain(" ".repeat(right + 1)));
            spans.push(Span::new("│", Style::new(Color::Dim)));
        }
        out.push(Line::spans(spans));
        if header_row {
            out.push(border("├", "┼", "┤"));
        }
    }
    out.push(border("└", "┴", "┘"));
    out
}

/// `|---|:--:|` → one alignment per column, or `None` when the row is not a separator.
fn parse_separator(row: &str) -> Option<Vec<Align>> {
    let cells = split_row(row);
    if cells.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    for cell in cells {
        let cell = cell.trim();
        let left = cell.starts_with(':');
        let right = cell.ends_with(':');
        let dashes = cell.trim_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
            return None;
        }
        out.push(match (left, right) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

/// Split `| a | b |` into its cells, tolerating a missing outer pipe.
fn split_row(row: &str) -> Vec<String> {
    let trimmed = row.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|cell| cell.trim().to_string()).collect()
}
