//! Code frames and table layout. Markdown syntax is handled by CommonMark once.

use crate::ui::text::{Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;
use pulldown_cmark::Alignment;
use unicode_segmentation::UnicodeSegmentation;

pub(super) fn frame(language: &str, body: &str, width: usize) -> Vec<Line> {
    let rows: Vec<_> = body
        .strip_suffix('\n')
        .unwrap_or(body)
        .split('\n')
        .collect();
    let label = (!language.is_empty()).then(|| language.trim_end_matches(',').to_string());
    let longest = rows.iter().map(|row| util::width(row)).max().unwrap_or(0);
    let wanted = (longest + 3).max(label.as_ref().map_or(0, |label| util::width(label) + 4));
    let bar_width = wanted.min(width.saturating_sub(1)).max(2);
    let label = label.and_then(|label| {
        let mut clipped = String::new();
        for grapheme in label.graphemes(true) {
            if util::width(&clipped) + util::width(grapheme) > bar_width.saturating_sub(4) {
                break;
            }
            clipped.push_str(grapheme);
        }
        (!clipped.is_empty()).then_some(clipped)
    });
    let bar = |corner: char, label: Option<&str>| {
        let mut spans = vec![Span::new(format!("{corner}─"), Style::new(Color::Dim))];
        let used = if let Some(label) = label {
            spans.push(Span::new(format!(" {label} "), Style::new(Color::Dim)));
            2 + util::width(label) + 2
        } else {
            2
        };
        spans.push(Span::new(
            "─".repeat(bar_width.saturating_sub(used)),
            Style::new(Color::Dim),
        ));
        Line::spans(spans)
    };
    let mut out = vec![bar('┌', label.as_deref())];
    out.extend(rows.into_iter().map(Line::plain));
    out.push(bar('└', None));
    out
}

pub(super) struct Table {
    pub rows: Vec<Vec<Vec<Span>>>,
    pub row: Vec<Vec<Span>>,
    pub cell: Vec<Span>,
    alignments: Vec<Alignment>,
}

impl Table {
    pub fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            rows: Vec::new(),
            row: Vec::new(),
            cell: Vec::new(),
            alignments,
        }
    }

    pub fn render(self, width: usize) -> Vec<Line> {
        let cols = self.alignments.len();
        let mut widths = vec![0usize; cols];
        for row in &self.rows {
            for (col, cell) in row.iter().take(cols).enumerate() {
                widths[col] = widths[col].max(cell_width(cell));
            }
        }
        let total = widths.iter().sum::<usize>() + 3 * cols + 1;
        if total > width || total > 100 {
            let Some(header) = self.rows.first() else {
                return Vec::new();
            };
            if self.rows.len() == 1 {
                let mut spans = Vec::new();
                for (index, cell) in header.iter().enumerate() {
                    if index > 0 {
                        spans.push(Span::plain("  "));
                    }
                    spans.extend(cell.clone());
                }
                return vec![Line::spans(spans)];
            }
            let mut lines = Vec::new();
            for (index, row) in self.rows.iter().skip(1).enumerate() {
                if index > 0 {
                    lines.push(Line::blank());
                }
                for (col, cell) in row.iter().enumerate() {
                    let mut spans = header.get(col).cloned().unwrap_or_default();
                    spans.push(Span::plain(": "));
                    spans.extend(cell.clone());
                    lines.push(Line::spans(spans));
                }
            }
            return lines;
        }
        let border = |left: &str, mid: &str, right: &str| {
            let mut text = left.to_string();
            for (col, width) in widths.iter().enumerate() {
                text.push_str(&"─".repeat(width + 2));
                text.push_str(if col + 1 == cols { right } else { mid });
            }
            Line::dim(text)
        };
        let mut lines = vec![border("┌", "┬", "┐")];
        for (index, row) in self.rows.iter().enumerate() {
            let mut spans = vec![Span::new("│", Style::new(Color::Dim))];
            for (col, width) in widths.iter().enumerate() {
                let cell = row.get(col).cloned().unwrap_or_default();
                let padding = width.saturating_sub(cell_width(&cell));
                let (left, right) = match self.alignments[col] {
                    Alignment::Right => (padding, 0),
                    Alignment::Center => (padding / 2, padding - padding / 2),
                    _ => (0, padding),
                };
                spans.push(Span::plain(" ".repeat(left + 1)));
                spans.extend(cell);
                spans.push(Span::plain(" ".repeat(right + 1)));
                spans.push(Span::new("│", Style::new(Color::Dim)));
            }
            lines.push(Line::spans(spans));
            if index == 0 {
                lines.push(border("├", "┼", "┤"));
            }
        }
        lines.push(border("└", "┴", "┘"));
        lines
    }
}

fn cell_width(cell: &[Span]) -> usize {
    cell.iter().map(|span| util::width(&span.text)).sum()
}
