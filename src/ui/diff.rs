//! Red/green diffs for `edit` and `write`.
//!
//! The diff is presentation only: the model gets a one-line summary, the user gets the
//! change. Rows keep a line number, and a big change is shown as an **excerpt of itself**
//! — the interesting part, taken from the real diff. It is never replaced by a sentence
//! saying a diff exists and has been left out: that is a row spent telling the user that
//! they were not shown the thing they asked for.

use similar::{ChangeTag, TextDiff};

use crate::tools::Display;
use crate::ui::screen::{Bg, Line, Span, Style};
use crate::ui::theme::{Color, Theme};
use crate::util;

/// Beyond these limits the diff is not worth the time it takes to compute, so the excerpt
/// is taken from the changed region only instead of from a diff of the whole thing. The
/// window is generous: it has to be big enough that the excerpt still shows real content.
const MAX_DIFF_BYTES: usize = 128 * 1024;
const MAX_DIFF_LINES: usize = 2000;

/// How much of a change too large to diff whole is actually diffed.
///
/// Both sides of the changed region are bounded by this, so the work stays proportional to
/// the excerpt rather than to the file. It is applied *after* the unchanged ends have been
/// trimmed away, so in the ordinary case — one edit in a big file — the region being diffed
/// is the edit itself, however large the file is.
const WINDOW_LINES: usize = 400;

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
    build(before, after)
}

pub fn for_write(before: &str, after: &str) -> Display {
    build(before, after)
}

/// A brand-new file has no "before", so every line is an addition.
pub fn for_new_file(content: &str) -> Display {
    build("", content)
}

fn build(before: &str, after: &str) -> Display {
    if before.len() + after.len() > MAX_DIFF_BYTES
        || before.lines().count() + after.lines().count() > MAX_DIFF_LINES
    {
        // Too big to diff as a whole, not too big to show: the excerpt comes from the part
        // that actually differs.
        return build_window(before, after);
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
    Display::Diff { diff: body_of(&rows), added, removed }
}

/// Render rows to the plain string that travels with the tool result. The UI only has to
/// colourise it.
fn body_of(rows: &[DiffRow]) -> String {
    rows.iter()
        .map(|row| {
            let marker = match row.kind {
                Kind::Added => '+',
                Kind::Removed => '-',
                Kind::Context => ' ',
            };
            format!("{marker}{:>5} {:>5} │ {}", num(row.old_line), num(row.new_line), row.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Show an excerpt of a change too large to diff whole.
///
/// The excerpt comes from the region that actually differs, found by trimming the common
/// lines off both ends first. That trim is a linear scan of the two sides, not a diff, and it
/// is what makes the excerpt land on the change: for the ordinary case — one edit in a huge
/// file — the region left after trimming *is* the edit, however large the file around it is.
/// Sampling the head and the tail of the file instead would show two slices of unchanged
/// text, report `+0 −0`, and say nothing at all about what changed.
///
/// When even the changed region is too large to show — a file rewritten end to end — the
/// first and last part of it are diffed, which is how a large rewrite is read: what was
/// taken away at the top, what replaced it at the bottom.
///
/// The counts are of what is shown. Walking a two-million-line file to put an exact number
/// in a header is the cost this limit exists to avoid, and for an excerpt the number of rows
/// on screen is the honest one.
fn build_window(before: &str, after: &str) -> Display {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let (head, tail) = common_ends(&before_lines, &after_lines);
    let old_middle = &before_lines[head..before_lines.len() - tail];
    let new_middle = &after_lines[head..after_lines.len() - tail];
    let mut rows: Vec<DiffRow> = Vec::new();
    for (old_start, new_start, old, new) in windows(old_middle, new_middle, head) {
        rows.extend(diff_rows(old, new, old_start, new_start));
    }
    let added = rows.iter().filter(|row| row.kind == Kind::Added).count();
    let removed = rows.iter().filter(|row| row.kind == Kind::Removed).count();
    Display::Diff { diff: body_of(&rows), added, removed }
}

/// How many lines at each end of the two files are identical, counted from that end.
///
/// The shared run is clipped to the shorter file so a file that is a prefix of the other
/// cannot make the two counts overlap and "trim" lines that are not shared.
fn common_ends(before: &[&str], after: &[&str]) -> (usize, usize) {
    let mut head = 0usize;
    while head < before.len() && head < after.len() && before[head] == after[head] {
        head += 1;
    }
    let most = before.len().min(after.len()) - head;
    let mut tail = 0usize;
    while tail < most && before[before.len() - 1 - tail] == after[after.len() - 1 - tail] {
        tail += 1;
    }
    (head, tail)
}

/// The parts of each side to diff, as `(old_start, new_start, old, new)` with the starts
/// already offset back to line numbers in the real file.
fn windows<'s, 'a>(
    old: &'s [&'a str],
    new: &'s [&'a str],
    offset: usize,
) -> Vec<(usize, usize, &'s [&'a str], &'s [&'a str])> {
    let old_ranges = ranges(old.len());
    let new_ranges = ranges(new.len());
    let count = old_ranges.len().max(new_ranges.len());
    (0..count)
        .map(|index| {
            // A side with nothing left to split off contributes an empty slice at its end,
            // so its lines are not shown twice.
            let (old_from, old_to) =
                old_ranges.get(index).copied().unwrap_or((old.len(), old.len()));
            let (new_from, new_to) =
                new_ranges.get(index).copied().unwrap_or((new.len(), new.len()));
            (
                offset + old_from,
                offset + new_from,
                &old[old_from..old_to],
                &new[new_from..new_to],
            )
        })
        .collect()
}

/// Which line ranges of a side to show: all of it, or its two ends when it does not fit.
fn ranges(len: usize) -> Vec<(usize, usize)> {
    if len <= WINDOW_LINES {
        return vec![(0, len)];
    }
    let half = WINDOW_LINES / 2;
    vec![(0, half), (len - half, len)]
}

/// Diff two slices, numbering rows from the offsets they have in the real file.
fn diff_rows(old: &[&str], new: &[&str], old_start: usize, new_start: usize) -> Vec<DiffRow> {
    // The joined sides are named bindings rather than inline temporaries: the diff borrows
    // them, and a temporary would be dropped at the end of the `let diff` statement.
    let old_text = old.join("\n");
    let new_text = new.join("\n");
    let diff = TextDiff::from_lines(&old_text, &new_text);
    diff.iter_all_changes()
        .map(|change| {
            let kind = match change.tag() {
                ChangeTag::Insert => Kind::Added,
                ChangeTag::Delete => Kind::Removed,
                ChangeTag::Equal => Kind::Context,
            };
            DiffRow {
                kind,
                old_line: change.old_index().map(|i| old_start + i + 1),
                new_line: change.new_index().map(|i| new_start + i + 1),
                text: change.value().trim_end_matches('\n').to_string(),
            }
        })
        .collect()
}

fn num(value: Option<usize>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}

/// Turn a diff payload into styled lines: the counts row and the excerpt under it.
///
/// Widths are measured in display columns, so the background tint reaches the edge of the
/// terminal no matter what the diff contains.
pub fn render(theme: &Theme, display: &Display, width: usize) -> Vec<Line> {
    let Display::Diff { diff, added, removed, .. } = display else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.push(Line::spans(vec![
        Span::plain("  "),
        Span::new(format!("+{added}"), Style::new(Color::DiffAddedText)),
        Span::plain(" "),
        Span::new(format!("−{removed}"), Style::new(Color::DiffRemovedText)),
    ]));
    let rows: Vec<&str> = diff.split('\n').collect();
    for row in &rows {
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
    let _ = theme;
    out
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

    /// The counts carried alongside the rows.
    fn counts(display: &Display) -> (usize, usize) {
        match display {
            Display::Diff { added, removed, .. } => (*added, *removed),
            _ => (0, 0),
        }
    }

    #[test]
    fn a_small_change_keeps_both_sides() {
        let display = for_edit("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(counts(&display), (1, 1));
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
        assert_eq!(counts(&display), (2, 0));
    }

    #[test]
    fn a_change_too_big_to_diff_whole_still_produces_rows() {
        // The old behaviour replaced the whole diff with a sentence saying a diff existed.
        // The user asked for the change; being told they were not shown it is not an answer.
        // The payload now carries real rows from the changed region, with the line numbers of
        // the real file.
        let before: String = (0..MAX_DIFF_LINES).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..MAX_DIFF_LINES).map(|i| format!("new {i}\n")).collect();
        let display = for_edit(&before, &after);
        let rendered = rows(&display);
        assert!(rendered.iter().any(|row| row.contains("old 0")), "{}", rendered.len());
        assert!(rendered.iter().any(|row| row.contains("new 1999")), "{}", rendered.len());
        // The window bounds the work: at most both windows, and each changed line on both
        // sides of it.
        assert!(rendered.len() <= WINDOW_LINES * 4, "the window bounds the work: {}", rendered.len());
        // The counts describe the rows that are actually there, since the whole file was not
        // walked to produce an exact total for an excerpt.
        assert_eq!(counts(&display), (WINDOW_LINES, WINDOW_LINES));
        // Nothing says the preview was skipped.
        assert!(!rendered.iter().any(|row| row.contains("省略")), "{}", rendered.len());
    }

    #[test]
    fn one_edited_line_in_a_huge_file_is_the_excerpt() {
        // The case the two-end window got wrong: an edit in the middle of a big file showed
        // two slices of *unchanged* text, reported `+0 −0`, and said nothing about the edit
        // at all. Trimming the shared ends first makes the excerpt be the change itself.
        let mut before: Vec<String> = (0..MAX_DIFF_LINES + 1000).map(|i| format!("line {i}")).collect();
        let mut after = before.clone();
        let middle = before.len() / 2;
        before[middle] = "OLD MIDDLE".to_string();
        after[middle] = "NEW MIDDLE".to_string();
        let display = for_edit(&before.join("\n"), &after.join("\n"));

        assert_eq!(counts(&display), (1, 1), "one line changed on each side");
        let rendered = rows(&display);
        assert!(rendered.iter().any(|row| row.contains("OLD MIDDLE")), "{rendered:?}");
        assert!(rendered.iter().any(|row| row.contains("NEW MIDDLE")), "{rendered:?}");
        // The line numbers still point where the rows came from in the real file.
        let expected = middle + 1;
        assert!(
            rendered.iter().any(|row| row.contains(&format!("{expected}"))),
            "line {expected} is the one that changed: {rendered:?}"
        );
    }

    #[test]
    fn an_appended_line_at_the_very_end_is_shown() {
        // An append is the common shape of a large write, and the changed region is the last
        // line — which a window counted from the start would miss entirely.
        let before: Vec<String> = (0..MAX_DIFF_LINES + 1000).map(|i| format!("line {i}")).collect();
        let mut after = before.clone();
        after.push("APPENDED".to_string());
        let display = for_edit(&before.join("\n"), &after.join("\n"));
        assert_eq!(counts(&display), (1, 0));
        assert!(rows(&display).iter().any(|row| row.contains("APPENDED")), "{:?}", rows(&display));
    }

    #[test]
    fn a_file_that_gained_a_prefix_shows_the_first_line() {
        // The mirror image: the change is at the very top, and everything below it moved.
        let body: Vec<String> = (0..MAX_DIFF_LINES + 1000).map(|i| format!("line {i}")).collect();
        let mut after = vec!["HEADER".to_string()];
        after.extend(body.clone());
        let display = for_edit(&body.join("\n"), &after.join("\n"));
        assert_eq!(counts(&display), (1, 0));
        let rendered = rows(&display);
        assert!(rendered.iter().any(|row| row.contains("HEADER")), "{rendered:?}");
    }

    #[test]
    fn the_excerpt_of_a_whole_file_rewrite_shows_both_ends() {
        // When the changed region is the whole file there is no trim to do, and the excerpt
        // is the head and the tail of the rewrite: what was taken away at the top, what
        // replaced it at the bottom.
        let before: String = (0..MAX_DIFF_LINES).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..MAX_DIFF_LINES).map(|i| format!("new {i}\n")).collect();
        let rendered = rows(&for_edit(&before, &after));
        assert!(rendered.iter().any(|row| row.contains("old 0")), "the first removal: {rendered:?}");
        assert!(
            rendered.iter().any(|row| row.contains("new 1999")),
            "the last addition: {rendered:?}"
        );
        // And the line numbers are the real ones, not the excerpt's own offsets.
        assert!(rendered.iter().any(|row| row.starts_with("-    1")), "{rendered:?}");
        assert!(
            rendered.iter().any(|row| row.contains(&format!("{:>5} │ new 1999", MAX_DIFF_LINES))),
            "the last addition keeps its line number: {rendered:?}"
        );
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
