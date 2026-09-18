//! Red/green diffs for `edit` and `write`.
//!
//! The diff is presentation only: the model gets a one-line summary, the user gets the
//! change. Rows keep a line number and the preview is capped, with a note saying how
//! many rows are hidden. Very large writes skip the computation entirely rather than
//! stalling the turn.

use similar::{ChangeTag, TextDiff};

use crate::config::Defaults;
use crate::tools::Display;
use crate::ui::screen::{Bg, Line, Span, Style};
use crate::ui::theme::{Color, Theme};
use crate::util;

/// Beyond these limits the diff is not worth the rows or the time.
const MAX_DIFF_BYTES: usize = 128 * 1024;
const MAX_DIFF_LINES: usize = 2000;

#[derive(Debug, Clone)]
pub struct DiffRow {
    pub kind: Kind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Added,
    Removed,
    Context,
}

/// Build the display payload for a replacement.
pub fn for_edit(before: &str, after: &str) -> Display {
    build(before, after, false)
}

pub fn for_write(before: &str, after: &str) -> Display {
    build(before, after, false)
}

/// A brand-new file has no "before", so every line is an addition.
pub fn for_new_file(content: &str) -> Display {
    build("", content, true)
}

fn build(before: &str, after: &str, _is_new: bool) -> Display {
    if before.len() + after.len() > MAX_DIFF_BYTES
        || before.lines().count() + after.lines().count() > MAX_DIFF_LINES
    {
        return Display::Diff { diff: String::new(), added: 0, removed: 0, omitted: true };
    }
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut rows: Vec<DiffRow> = Vec::new();
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Insert => {
                added += 1;
                Kind::Added
            }
            ChangeTag::Delete => {
                removed += 1;
                Kind::Removed
            }
            ChangeTag::Equal => Kind::Context,
        };
        let text = change.value().trim_end_matches('\n').to_string();
        rows.push(DiffRow {
            kind,
            old_line: change.old_index().map(|i| i + 1),
            new_line: change.new_index().map(|i| i + 1),
            text,
        });
    }
    // Render eagerly to a plain string: the payload travels with the tool result and the
    // UI only has to colourise it.
    let body = rows
        .iter()
        .map(|row| {
            let marker = match row.kind {
                Kind::Added => '+',
                Kind::Removed => '-',
                Kind::Context => ' ',
            };
            format!("{marker}{:>5} {:>5} │ {}", num(row.old_line), num(row.new_line), row.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Display::Diff { diff: body, added, removed, omitted: false }
}

fn num(value: Option<usize>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}

/// Turn a diff payload into styled lines: summary, the preview rows, and the hidden-rows
/// note. Widths are measured in display columns, so the background tint reaches the edge
/// of the terminal no matter what the diff contains.
pub fn render(theme: &Theme, display: &Display, width: usize) -> Vec<Line> {
    let Display::Diff { diff, added, removed, omitted } = display else {
        return Vec::new();
    };
    if *omitted {
        return vec![Line::new("  文件较大，差异预览已省略。", Style::new(Color::Dim))];
    }
    let mut out = Vec::new();
    out.push(Line::spans(vec![
        Span::plain("  "),
        Span::new(format!("+{added}"), Style::new(Color::DiffAddedText)),
        Span::plain(" "),
        Span::new(format!("−{removed}"), Style::new(Color::DiffRemovedText)),
    ]));
    let rows: Vec<&str> = diff.split('\n').collect();
    let (visible, hidden) = select(&rows);
    for row in visible {
        let marker = row.chars().next();
        // No padding here: the screen pads to the terminal width with the row's background,
        // which keeps the tint a solid bar without making the text any longer than it is.
        let text = format!(" {}", util::truncate(row, width.saturating_sub(1), "…"));
        let (style, fill) = match marker {
            Some('+') => (Style::with_bg(Color::DiffAddedText, Bg::Added), Bg::Added),
            Some('-') => (Style::with_bg(Color::DiffRemovedText, Bg::Removed), Bg::Removed),
            _ => (Style::new(Color::Dim), Bg::None),
        };
        out.push(Line::spans(vec![Span::with_fill(text, style, fill)]));
    }
    if hidden > 0 {
        out.push(Line::new(
            format!("  … 已收起 {hidden} 行 · 按 Ctrl+O 展开"),
            Style::new(Color::Dim),
        ));
    }
    let _ = theme;
    out
}

/// Choose the preview rows. A large rewrite shows both sides; a purely additive change
/// starts just before the first interesting row.
fn select<'a>(rows: &[&'a str]) -> (Vec<&'a str>, usize) {
    if rows.len() <= Defaults::DIFF_PREVIEW_LINES {
        return (rows.to_vec(), 0);
    }
    let removed: Vec<&&str> = rows.iter().filter(|row| row.starts_with('-')).collect();
    let added: Vec<&&str> = rows.iter().filter(|row| row.starts_with('+')).collect();
    if !removed.is_empty() && !added.is_empty() {
        let half = Defaults::DIFF_PREVIEW_LINES / 2;
        let mut chosen: Vec<&str> = Vec::new();
        chosen.extend(removed.iter().take(half).map(|row| **row));
        chosen.extend(added.iter().take(half).map(|row| **row));
        let hidden = rows.len() - chosen.len();
        return (chosen, hidden);
    }
    let start = rows
        .iter()
        .position(|row| !row.starts_with(' '))
        .map(|index| index.saturating_sub(2))
        .unwrap_or(0);
    let end = (start + Defaults::DIFF_PREVIEW_LINES).min(rows.len());
    (rows[start..end].to_vec(), rows.len() - (end - start))
}

/// The one-line summary used by the transcript header.
pub fn summary(display: &Display) -> Option<(usize, usize, bool)> {
    match display {
        Display::Diff { added, removed, omitted, .. } => Some((*added, *removed, *omitted)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(display: &Display) -> Vec<String> {
        match display {
            Display::Diff { diff, .. } => diff.split('\n').map(str::to_string).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_small_change_keeps_both_sides() {
        let display = for_edit("a\nb\nc\n", "a\nB\nc\n");
        let (added, removed, omitted) = summary(&display).unwrap();
        assert_eq!((added, removed, omitted), (1, 1, false));
        let rendered = rows(&display);
        // Both the old and the new line survive, each with its own line number, and the
        // context rows line up on both sides.
        assert!(rendered.contains(&"     1     1 │ a".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"-    2       │ b".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"+          2 │ B".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"     3     3 │ c".to_string()), "{rendered:?}");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let display = for_new_file("one\ntwo\n");
        let (added, removed, _) = summary(&display).unwrap();
        assert_eq!((added, removed), (2, 0));
    }

    #[test]
    fn a_large_change_is_summarised_rather_than_computed() {
        let before = "x\n".repeat(MAX_DIFF_LINES);
        let after = "y\n".repeat(MAX_DIFF_LINES);
        let display = for_edit(&before, &after);
        assert!(summary(&display).unwrap().2, "the preview should be omitted");
        let theme = Theme { mode: crate::ui::theme::ColorMode::Ansi256 };
        let lines = render(&theme, &display, 80);
        assert!(lines[0].text().contains("已省略"));
    }

    #[test]
    fn the_preview_is_capped_and_says_how_much_is_hidden() {
        let before: String = (0..100).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..100).map(|i| format!("new {i}\n")).collect();
        let display = for_edit(&before, &after);
        let theme = Theme { mode: crate::ui::theme::ColorMode::Ansi256 };
        let lines = render(&theme, &display, 80);
        let plain: Vec<String> = lines.iter().map(Line::text).collect();
        assert!(plain.iter().any(|line| line.contains("已收起")), "{plain:?}");
        // summary row + preview rows + the hidden note
        assert!(plain.len() <= Defaults::DIFF_PREVIEW_LINES + 2);
    }

    #[test]
    fn rendering_keeps_rows_within_the_width() {
        let display = for_edit("short\n", "a much longer replacement line that overflows\n");
        let theme = Theme { mode: crate::ui::theme::ColorMode::True };
        for line in render(&theme, &display, 20) {
            assert!(line.width() <= 20, "too wide: {line:?}");
        }
    }
}
