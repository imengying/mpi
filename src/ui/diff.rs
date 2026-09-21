//! Red/green diffs for `edit` and `write`.
//!
//! The diff is presentation only: the model gets a one-line summary, the user gets the
//! change. Rows carry one line number each — the row's place in the file as it now stands —
//! and a big change is shown as an **excerpt of itself**, never replaced by a sentence saying
//! a diff exists and has been left out: that is a row spent telling the user that they were
//! not shown the thing they asked for.
//!
//! What bounds the work is the *change*, not the file. Unchanged runs are trimmed to a few
//! lines of context, so a one-line edit in a large file produces a handful of rows rather
//! than one row per line of the file — which matters twice over, because the block that draws
//! them keeps only its head and its tail when collapsed, and a payload the size of the file
//! would hide the change in the middle it drops.

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
    /// The line number this row shows, in one column.
    ///
    /// One number, not `git diff`'s two. A row belongs to one side of the change, so it has
    /// exactly one number worth showing, and the other column was blank on every single row:
    /// two columns spent the width twice and put the numbers on opposite sides from row to
    /// row, so reading down a hunk meant chasing them across the screen.
    ///
    /// The number is the row's line in the file as it stands *now* — that is what the reader
    /// is locating themselves in. Context and added rows carry their new line. A removed line
    /// is not in the file any more, so it keeps the number it had, which is where the reader
    /// last saw it; that is why a removal can name a line above the addition that replaced it.
    pub line: Option<usize>,
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
    let before_lines = lines_with_endings(before);
    let after_lines = lines_with_endings(after);
    let rows = diff_rows(&before_lines, &after_lines, 0, 0);
    let added = rows.iter().filter(|row| row.kind == Kind::Added).count();
    let removed = rows.iter().filter(|row| row.kind == Kind::Removed).count();
    Display::Diff { diff: body_of(&rows), added, removed }
}

/// A file's lines *with* their newline terminators.
///
/// The terminator has to travel with the line: a diff of `"a\nb"` against `"a\nb\n"` is a
/// change to the last line, and a diff of the same two texts reconstructed by joining
/// newline-stripped lines is not — joining loses the difference, and the renderer then shows
/// an unchanged line as both removed and added. Keeping each line's own terminator means the
/// slices can be concatenated back into exactly the text they came from, so what the diff
/// sees is what is on disk.
fn lines_with_endings(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Rows of unchanged text to keep before and after each change.
///
/// Enough to read the change in its surroundings, and no more. The whole file used to be
/// kept as context, which is what a diff *is* — but it also means a one-line edit in a
/// 400-line file produces a 401-row payload, and the collapsed preview only draws the top
/// and the bottom of it. The edit that the block exists to show was the one row that never
/// made it to the screen. Three lines is the convention every diff tool converged on, and
/// with a bounded run the payload is proportional to the change rather than to the file.
const CONTEXT_LINES: usize = 3;

/// Flush the pending run of unchanged rows, keeping the ends that are worth reading.
///
/// The run is held back rather than emitted as it arrives because how much of it is worth
/// keeping depends on what comes next. A run that leads into a change keeps its *last*
/// [`CONTEXT_LINES`] rows — the lines immediately above the change are its context, and the
/// lines above those are the rest of the file. A run that sits between two changes is both
/// the"after" of the first and the "before" of the second, so it keeps its first and last
/// [`CONTEXT_LINES`] rows and drops the middle: those are the rows next to each change, and
/// the rows in the middle of a long gap are the ones nobody is reading.
fn push_context(rows: &mut Vec<DiffRow>, context: &mut Vec<DiffRow>) {
    if context.is_empty() {
        return;
    }
    if rows.is_empty() {
        // Nothing stands before this run, so it is the top of the file leading into a change.
        let keep = context.len().saturating_sub(CONTEXT_LINES);
        context.drain(..keep);
    } else if context.len() > CONTEXT_LINES * 2 {
        // Between two changes: keep the rows against each one, drop the gap in between.
        context.drain(CONTEXT_LINES..context.len() - CONTEXT_LINES);
    }
    rows.append(context);
}

/// The single number a row shows, from the indices the diff reports.
///
/// Context and added rows live in the file as it now stands, so they show the new number. A
/// removed row does not exist there any more, so it shows the old one — the line the reader
/// last saw it on. Both sides are 0-based here and 1-based on screen.
fn row_line(kind: Kind, old: Option<usize>, new: Option<usize>) -> Option<usize> {
    let picked = match kind {
        Kind::Removed => old.or(new),
        Kind::Added | Kind::Context => new.or(old),
    };
    picked.map(|index| index + 1)
}

/// Render rows to the plain string that travels with the tool result. The UI only has to
/// colourise it. The number is already chosen per row (see [`DiffRow::line`]).
fn body_of(rows: &[DiffRow]) -> String {
    rows.iter()
        .map(|row| {
            let marker = match row.kind {
                Kind::Added => '+',
                Kind::Removed => '-',
                Kind::Context => ' ',
            };
            format!("{marker}{:>5} │ {}", num(row.line), row.text)
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
    let before_lines = lines_with_endings(before);
    let after_lines = lines_with_endings(after);
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
///
/// The slices carry their own line terminators (see [`lines_with_endings`]), so they are
/// concatenated rather than re-joined: joining with a separator would add a terminator the
/// text may not have had, and a final line without one would compare equal to a final line
/// with one.
///
/// Runs of unchanged rows are trimmed to [`CONTEXT_LINES`] at each end, exactly as in
/// [`build`]: the same rule applies whether the sides diffed are two files or two windows
/// taken out of large ones, and a window that kept all of its context would reinstate the
/// problem the trim exists to solve.
fn diff_rows(old: &[&str], new: &[&str], old_start: usize, new_start: usize) -> Vec<DiffRow> {
    // The joined sides are named bindings rather than inline temporaries: the diff borrows
    // them, and a temporary would be dropped at the end of the `let diff` statement.
    let old_text = old.concat();
    let new_text = new.concat();
    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut context: Vec<DiffRow> = Vec::new();
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Insert => Kind::Added,
            ChangeTag::Delete => Kind::Removed,
            ChangeTag::Equal => Kind::Context,
        };
        let row = DiffRow {
            kind,
            line: row_line(
                kind,
                change.old_index().map(|i| old_start + i),
                change.new_index().map(|i| new_start + i),
            ),
            text: change.value().trim_end_matches('\n').to_string(),
        };
        if kind == Kind::Context {
            context.push(row);
        } else {
            push_context(&mut rows, &mut context);
            rows.push(row);
        }
    }
    if !rows.is_empty() {
        context.truncate(CONTEXT_LINES);
        rows.append(&mut context);
    }
    rows
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
        // Both the old and the new line survive, with one line number each — the number the
        // row has in the file as it now stands, so the column reads straight down the screen.
        assert!(rendered.contains(&"     1 │ a".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"-    2 │ b".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"+    2 │ B".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"     3 │ c".to_string()), "{rendered:?}");
    }

    #[test]
    fn every_row_carries_exactly_one_number_in_one_column() {
        // The layout this replaces was `git diff`'s: an old-number column and a new-number
        // column, one of which is blank on every single row. On a narrow terminal that spent
        // the width twice and made the numbers alternate sides down a hunk. There is one
        // column now, and every row puts its number in it.
        let display = for_edit("a\nb\nc\nd\n", "a\nB\nc\nD\n");
        let rendered = rows(&display);
        for row in &rendered {
            let (marker, rest) = row.split_at(1);
            assert!(matches!(marker, "+" | "-" | " "), "unknown marker: {row:?}");
            let (number, tail) = rest
                .split_once(" │")
                .unwrap_or_else(|| panic!("no column: {row:?}"));
            assert_eq!(number.len(), 5, "the column is a fixed width: {row:?}");
            let value: usize = number
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("not a number: {row:?}"));
            assert!(value >= 1, "line numbers start at 1: {row:?}");
            assert!(tail.starts_with(' '), "the text is past the column: {row:?}");
        }
        assert!(rendered.len() >= 4, "{rendered:?}");
    }

    #[test]
    fn the_column_reads_straight_down_the_rows_that_are_in_the_file() {
        // What makes one column worth reading is that the numbers mean one thing: where this
        // row is in the file *now*. Rows that are in the file — context and additions — must
        // therefore increase down the screen. Removals are the deliberate exception: they
        // name a line that is gone, so they keep the number it had, which can sit above the
        // number of the addition that replaced it (delete 2414–2417, add 2414 again).
        let before: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let after = "line 1\nline 2\nline 3\nline 4\nREPLACEMENT\nline 9\nline 10\n";
        let rendered = rows(&for_edit(&before, after));

        let mut last = 0usize;
        for row in &rendered {
            let in_file = !row.starts_with('-');
            let number: usize = row[1..6].trim().parse().unwrap();
            if in_file {
                assert!(number > last, "a row in the file moves forward: {row:?}");
                last = number;
            }
        }
        // The removals still name the real lines they were, and the replacement names the
        // line it took over.
        assert!(rendered.contains(&"-    5 │ line 5".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"-    8 │ line 8".to_string()), "{rendered:?}");
        assert!(rendered.contains(&"+    5 │ REPLACEMENT".to_string()), "{rendered:?}");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let display = for_new_file("one\ntwo\n");
        assert_eq!(counts(&display), (2, 0));
    }

    #[test]
    fn unchanged_lines_around_a_change_are_trimmed_not_kept_whole() {
        // The payload used to be the whole file with one line changed — 401 rows for a
        // 400-line file. The block that draws it keeps a head and a tail, so the one row that
        // mattered, the edit, was the row that fell in the hidden middle: the block showed the
        // top of the file and the bottom of the file and nothing about the change at all.
        // Trimming the unchanged runs is what makes the payload proportional to the change,
        // and what puts the change on screen.
        let mut before: Vec<String> = (1..=400).map(|i| format!("line {i}\n")).collect();
        let mut after = before.clone();
        before[250] = "OLD LINE 251\n".into();
        after[250] = "NEW LINE 251\n".into();
        let display = for_edit(&before.concat(), &after.concat());
        let rendered = rows(&display);

        assert_eq!(counts(&display), (1, 1));
        assert!(
            rendered.iter().any(|row| row.contains("OLD LINE 251")),
            "the change is in the payload: {rendered:?}"
        );
        // Three rows of context on each side of the one-line change, so ten rows in all: the
        // change is nowhere near a screenful, whatever the file's size.
        assert_eq!(rendered.len(), CONTEXT_LINES * 2 + 2, "{rendered:?}");
        assert!(!rendered.iter().any(|row| row.ends_with("line 1")), "{rendered:?}");
    }

    #[test]
    fn a_gap_between_two_changes_keeps_the_rows_next_to_each_one() {
        // Both changes have to survive, so the rows against each of them are kept and the
        // empty middle of the gap is dropped. Keeping the head of the gap and cutting its
        // tail would leave the second change with no context at all.
        let mut before: Vec<String> = (1..=200).map(|i| format!("line {i}\n")).collect();
        let mut after = before.clone();
        before[10] = "OLD A\n".into();
        after[10] = "NEW A\n".into();
        before[150] = "OLD B\n".into();
        after[150] = "NEW B\n".into();
        let rendered = rows(&for_edit(&before.concat(), &after.concat()));

        assert!(rendered.iter().any(|row| row.contains("OLD A")), "{rendered:?}");
        assert!(rendered.iter().any(|row| row.contains("OLD B")), "{rendered:?}");
        assert!(rendered.iter().any(|row| row.ends_with("line 10")), "context above A");
        assert!(rendered.iter().any(|row| row.ends_with("line 12")), "context below A");
        assert!(rendered.iter().any(|row| row.ends_with("line 149")), "context above B");
        assert!(rendered.iter().any(|row| row.ends_with("line 153")), "context below B");
    }

    #[test]
    fn the_context_at_the_top_of_a_file_is_what_led_into_the_change() {
        // Nothing precedes the run, so it is the file's opening: the lines that lead into the
        // change are its tail, and it is the head of a long opening that is dropped.
        let mut before: Vec<String> = (1..=50).map(|i| format!("line {i}\n")).collect();
        let mut after = before.clone();
        before[40] = "OLD\n".into();
        after[40] = "NEW\n".into();
        let rendered = rows(&for_edit(&before.concat(), &after.concat()));

        assert!(rendered.iter().any(|row| row.contains("OLD")), "{rendered:?}");
        assert!(rendered.iter().any(|row| row.ends_with("line 38")), "the lead-in survives");
        assert!(
            !rendered.iter().any(|row| row.contains("line 1\n")),
            "the top of the file is not context for anything: {rendered:?}"
        );
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
        let before: String = (0..MAX_DIFF_LINES + 1000).map(|i| format!("line {i}\n")).collect();
        let after = format!("{before}APPENDED\n");
        let display = for_edit(&before, &after);
        assert_eq!(counts(&display), (1, 0));
        assert!(rows(&display).iter().any(|row| row.contains("APPENDED")), "{:?}", rows(&display));
    }

    #[test]
    fn a_last_line_that_lost_its_newline_is_a_change() {
        // Text with no terminator on its final line differs from text that has one, and the
        // user can see the difference in an editor. Rebuilding each side by joining
        // newline-stripped lines discarded it, and the line that only differed in its ending
        // came out as both removed and added — the same text on two tinted rows. Keeping each
        // line's terminator with it is what makes these two texts compare as they read.
        let joined = for_edit("a\nb", "a\nb\n");
        assert_eq!(counts(&joined), (1, 1), "the final line changed: {:?}", rows(&joined));

        let same = for_edit("a\nb\n", "a\nb\n");
        assert_eq!(counts(&same), (0, 0), "identical files differ in nothing");
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
