//! What the user is typing: the line editor and the layout of its rows.
//!
//! The editor is the buffer and the caret; the layout is what turns that buffer into display
//! rows. Both are pure — no terminal, no `Screen` — so the caret can be checked as a
//! coordinate rather than by watching a cursor blink.
//!
//! The caret is a character index. Movement and deletion follow grapheme boundaries,
//! so combining marks and emoji sequences stay intact.

use crate::util;
use unicode_segmentation::UnicodeSegmentation;

/// One row of the input area: the prompt prefix, the text on that row, and the index into
/// the buffer (in characters) of the first character the row shows.
///
/// The start index is what makes a caret possible: `caret` is a character index into the
/// buffer, and this is what turns it into a row and a column.
pub(crate) struct InputRow {
    pub(crate) prefix: String,
    pub(crate) text: String,
    pub(crate) start: usize,
}

impl InputRow {
    fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// Split the input buffer into display rows, each tagged with its prompt prefix.
///
/// Wrapping is done on the buffer as a whole with the prompt width subtracted, then the
/// prompt is prefixed to the first row, so a long word breaks at the real screen edge
/// instead of two columns early on every row. Every row carries a prefix string —
/// `"> "` for the first, spaces for the rest — which is what keeps the continuation rows
/// aligned under the text rather than under the prompt.
///
/// The break is by character width, not by word: an input line is not prose, and moving a
/// partly-typed path or command onto its own row would make the caret jump around.
pub(crate) fn input_layout(text: &str, width: usize, prompt_width: usize) -> Vec<InputRow> {
    let mut rows: Vec<InputRow> = Vec::new();
    let mut current = String::new();
    // Character index of the first character of `current`, and of the next one to place.
    let mut start = 0usize;
    let mut used = 0usize;
    // Every row, continuation included, carries a two-column prefix so the text lines up
    // under itself; the usable width is therefore the same on all of them.
    let room = width.saturating_sub(prompt_width).max(1);
    let mut index = 0;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            rows.push(InputRow {
                prefix: String::new(),
                text: std::mem::take(&mut current),
                start,
            });
            if used == room {
                rows.push(InputRow {
                    prefix: String::new(),
                    text: String::new(),
                    start: index,
                });
            }
            start = index + 1;
            index += 1;
            used = 0;
            continue;
        }
        let cw = util::width(grapheme);
        if used + cw > room && used > 0 {
            rows.push(InputRow {
                prefix: String::new(),
                text: std::mem::take(&mut current),
                start,
            });
            start = index;
            used = 0;
        }
        current.push_str(grapheme);
        used += cw;
        index += grapheme.chars().count();
    }
    rows.push(InputRow {
        prefix: String::new(),
        text: current,
        start,
    });
    // A row filled to the last cell needs one more for the caret to sit on. The caret marks
    // where the next character goes, and there is no cell left on this row to draw it in:
    // asking the terminal for a column past the right edge is clamped to the last cell at
    // best, and clamps are exactly what a caret must not depend on. An empty row is where
    // readline puts the cursor in the same situation.
    if rows
        .last()
        .is_some_and(|row| util::width(&row.text) == room)
    {
        let start = text.chars().count();
        rows.push(InputRow {
            prefix: String::new(),
            text: String::new(),
            start,
        });
    }
    // Tag the prefixes now that the row count is known.
    for (index, row) in rows.iter_mut().enumerate() {
        row.prefix = if prompt_width == 0 {
            String::new()
        } else if index == 0 {
            "› ".to_string()
        } else {
            " ".repeat(prompt_width)
        };
    }
    rows
}

/// Where the caret belongs, as `(row, columns into the text)`, for a character index into
/// the buffer.
///
/// The row is the last one that starts at or before the caret, which puts a caret landing
/// exactly on a wrap boundary at the start of the row below — where a terminal would put it
/// after writing the last cell of a full line. The column is a *display* width, measured
/// over the characters before the caret, so a CJK character counts as the two cells it
/// occupies.
pub(crate) fn input_caret(rows: &[InputRow], caret: usize) -> (usize, usize) {
    let row = rows.iter().rposition(|row| row.start <= caret).unwrap_or(0);
    let offset = caret.saturating_sub(rows[row].start).min(rows[row].chars());
    let shown: String = rows[row].text.chars().take(offset).collect();
    (row, util::width(&shown))
}

/// The rows of the input buffer as `(prefix, text)` pairs, as the tests assert on them.
#[cfg(test)]
fn input_rows(text: &str, width: usize, prompt_width: usize) -> Vec<(String, String)> {
    input_layout(text, width, prompt_width)
        .into_iter()
        .map(|row| (row.prefix, row.text))
        .collect()
}

/// The longest prefix shared by every name, used to fill in as much as is unambiguous.
pub(crate) fn common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = first.to_string();
    while !names.iter().all(|name| name.starts_with(&prefix)) {
        // Drop one character at a time; the loop ends at the empty string, which every name
        // has as a prefix.
        prefix.pop();
    }
    prefix
}
/// The line editor's state: what has been typed and where the caret sits in it.
///
/// The caret is a **character** index, not a byte offset and not a column: bytes would split
/// a CJK character, and columns would make a horizontal move depend on how wide the
/// characters happen to be. Every edit goes through this struct, so an index can never be
/// left pointing into the middle of a character or past the end of the buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    chars: Vec<char>,
    /// Where the next character typed goes, as an index into `chars`.
    caret: usize,
    preferred_column: Option<usize>,
}

impl Editor {
    pub fn new() -> Self {
        Editor::default()
    }

    pub fn from_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let caret = chars.len();
        Editor {
            chars,
            caret,
            preferred_column: None,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Move `delta` graphemes left or right, stopping at both ends.
    ///
    /// Clamping rather than wrapping is what makes the key safe to hold down: a caret that
    /// jumped from one end of the line to the other would make correcting a typo a guessing
    /// game about where it is going to land.
    pub fn move_caret(&mut self, delta: isize) {
        self.preferred_column = None;
        let boundaries = self.boundaries();
        let index = boundaries.partition_point(|position| *position < self.caret);
        let next = index.saturating_add_signed(delta).min(boundaries.len() - 1);
        self.caret = boundaries[next];
    }

    pub fn home(&mut self) {
        self.preferred_column = None;
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.preferred_column = None;
        self.caret = self.chars.len();
    }

    /// Insert at the caret, which then sits after the text just typed.
    pub fn insert(&mut self, text: &str) {
        self.preferred_column = None;
        self.chars.splice(self.caret..self.caret, text.chars());
        self.caret += text.chars().count();
        self.caret = self
            .boundaries()
            .into_iter()
            .find(|position| *position >= self.caret)
            .unwrap_or(self.chars.len());
    }

    /// Both clipboard paths insert one edit, preserving paragraphs without terminal controls.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert(&crate::util::sanitize(&text));
    }

    /// Backspace: remove the character *before* the caret, if there is one.
    pub fn backspace(&mut self) {
        self.preferred_column = None;
        if self.caret > 0 {
            let previous = self
                .boundaries()
                .into_iter()
                .take_while(|position| *position < self.caret)
                .last()
                .unwrap_or(0);
            self.chars.drain(previous..self.caret);
            self.caret = previous;
        }
    }

    /// Delete: remove the character *under* the caret, leaving the caret where it is.
    pub fn delete(&mut self) {
        self.preferred_column = None;
        if self.caret < self.chars.len() {
            let next = self
                .boundaries()
                .into_iter()
                .find(|position| *position > self.caret)
                .unwrap_or(self.chars.len());
            self.chars.drain(self.caret..next);
        }
    }

    /// Delete back to the start of the current word, as Ctrl+W does everywhere else.
    ///
    /// The caret decides what "the current word" is: it means the text before the caret, so
    /// pressing Ctrl+W in the middle of a line removes the word to the left rather than the
    /// tail of the line.
    pub fn delete_word(&mut self) {
        let end = self.caret;
        self.move_word(-1);
        self.chars.drain(self.caret..end);
    }

    pub fn move_word(&mut self, direction: isize) {
        self.preferred_column = None;
        if direction < 0 {
            while self.caret > 0 && self.chars[self.caret - 1].is_whitespace() {
                self.caret -= 1;
            }
            while self.caret > 0 && !self.chars[self.caret - 1].is_whitespace() {
                self.caret -= 1;
            }
        } else {
            while self.caret < self.chars.len() && !self.chars[self.caret].is_whitespace() {
                self.caret += 1;
            }
            while self.caret < self.chars.len() && self.chars[self.caret].is_whitespace() {
                self.caret += 1;
            }
        }
        let boundaries = self.boundaries();
        self.caret = if direction < 0 {
            boundaries
                .into_iter()
                .take_while(|position| *position <= self.caret)
                .last()
                .unwrap_or(0)
        } else {
            boundaries
                .into_iter()
                .find(|position| *position >= self.caret)
                .unwrap_or(self.chars.len())
        };
    }

    /// Delete from the caret back to the start of the line.
    pub fn delete_to_start(&mut self) {
        self.preferred_column = None;
        let start = self.line_start();
        self.chars.drain(start..self.caret);
        self.caret = start;
    }

    /// Delete from the caret to the end of the line.
    pub fn delete_to_end(&mut self) {
        self.preferred_column = None;
        let end = self.line_end();
        let end = if end == self.caret && end < self.chars.len() {
            end + 1
        } else {
            end
        };
        self.chars.drain(self.caret..end);
    }

    pub fn line_home(&mut self) {
        self.caret = self.line_start();
        self.preferred_column = None;
    }

    pub fn line_end_caret(&mut self) {
        self.caret = self.line_end();
        self.preferred_column = None;
    }

    fn line_start(&self) -> usize {
        self.chars[..self.caret]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.chars[self.caret..]
            .iter()
            .position(|ch| *ch == '\n')
            .map_or(self.chars.len(), |index| self.caret + index)
    }

    /// Move by visual rows, remembering the display column across short lines.
    /// Returns false at an edge, where the caller can enter input history.
    pub fn move_vertical(&mut self, delta: isize, width: usize, prefix: usize) -> bool {
        let rows = input_layout(&self.text(), width, prefix);
        let (row, column) = input_caret(&rows, self.caret);
        let target = row as isize + delta;
        if target < 0 || target >= rows.len() as isize {
            return false;
        }
        let column = *self.preferred_column.get_or_insert(column);
        let target = target as usize;
        let mut offset = 0;
        let mut cells = 0;
        for grapheme in rows[target].text.graphemes(true) {
            let next = cells + util::width(grapheme);
            if next > column {
                break;
            }
            cells = next;
            offset += grapheme.chars().count();
        }
        self.caret = rows[target].start + offset;
        // A full wrapped row's trailing insertion position belongs to the next row.
        if target + 1 < rows.len() && self.caret >= rows[target + 1].start {
            self.caret = self
                .boundaries()
                .into_iter()
                .take_while(|position| *position < rows[target + 1].start)
                .last()
                .unwrap_or(0);
        }
        true
    }

    fn boundaries(&self) -> Vec<usize> {
        let text = self.text();
        let mut boundaries = vec![0];
        let mut chars = 0;
        for grapheme in text.graphemes(true) {
            chars += grapheme.chars().count();
            boundaries.push(chars);
        }
        boundaries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_editing_keeps_accents_flags_and_joined_emoji_intact() {
        for cluster in ["e\u{301}", "🇨🇳", "👩‍💻"] {
            let mut editor = Editor::from_text(&format!("a{cluster}b"));
            editor.move_caret(-2);
            assert_eq!(editor.caret(), 1);
            editor.delete();
            assert_eq!(editor.text(), "ab");
            editor = Editor::from_text(&format!("a{cluster}"));
            editor.backspace();
            assert_eq!(editor.text(), "a");
            let rows = input_layout(&format!("{cluster}x{cluster}"), 5, 2);
            assert!(rows.iter().all(|row| util::width(&row.text) <= 3));
            assert_eq!(
                rows.iter().map(|row| row.text.as_str()).collect::<String>(),
                format!("{cluster}x{cluster}")
            );
        }
    }

    #[test]
    fn vertical_movement_remembers_columns_and_uses_wrapped_rows() {
        let mut editor = Editor::from_text("abcdef\n中\nuvwxyz");
        assert!(editor.move_vertical(-1, 20, 2));
        assert_eq!(editor.caret(), 8);
        assert!(editor.move_vertical(-1, 20, 2));
        assert_eq!(editor.caret(), 6);
        assert!(!editor.move_vertical(-1, 20, 2));
        assert!(editor.move_vertical(1, 20, 2));
        assert!(editor.move_vertical(1, 20, 2));
        assert_eq!(editor.caret(), editor.len());
        let mut editor = Editor::from_text("abcdefghijklmn");
        assert!(editor.move_vertical(-1, 8, 2));
        assert_eq!(editor.caret(), 8);
        assert!(editor.move_vertical(-1, 8, 2));
        assert_eq!(editor.caret(), 2);
    }

    #[test]
    fn line_editing_and_word_navigation_preserve_other_paragraphs() {
        let mut editor = Editor::from_text("before\none two\nafter");
        editor.move_caret(-8);
        editor.line_home();
        assert_eq!(editor.caret(), 7);
        editor.line_end_caret();
        assert_eq!(editor.caret(), 14);
        editor.move_word(-1);
        assert_eq!(editor.caret(), 11);
        assert_eq!(editor.text(), "before\none two\nafter");
        editor.delete_to_end();
        assert_eq!(editor.text(), "before\none \nafter");
        editor.delete_to_start();
        assert_eq!(editor.text(), "before\n\nafter");
        editor.delete_to_end();
        assert_eq!(editor.text(), "before\nafter");
    }

    #[test]
    fn pasted_paragraphs_keep_breaks_and_insert_at_the_caret() {
        let mut editor = Editor::from_text("前后");
        editor.move_caret(-1);
        editor.paste("甲\r\n\r\n乙\r丙\n");
        assert_eq!(editor.text(), "前甲\n\n乙\n丙\n后");
        assert_eq!(editor.caret(), 8);
        editor.paste("\u{1b}[31m红\u{1b}[0m\t字");
        assert_eq!(editor.text(), "前甲\n\n乙\n丙\n红    字后");
    }

    #[test]
    fn paragraph_layout_tracks_blank_lines_and_caret_positions() {
        let text = "你好\n\n世界\n";
        let rows = input_layout(text, 7, 2);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["你好", "", "世界", ""]
        );
        assert_eq!(input_caret(&rows, 2), (0, 4));
        assert_eq!(input_caret(&rows, 3), (1, 0));
        assert_eq!(input_caret(&rows, 4), (2, 0));
        assert_eq!(input_caret(&rows, 7), (3, 0));
        let rows = input_layout("abcde\nf", 6, 2);
        assert_eq!(
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
            vec!["abcd", "e", "f"]
        );
        assert_eq!(input_caret(&rows, 6), (2, 0));
        let rows = input_layout("你好\n", 6, 2);
        assert_eq!(input_caret(&rows, 2), (1, 0));
        assert_eq!(input_caret(&rows, 3), (2, 0));
    }

    #[test]
    fn left_and_right_step_by_character_and_stop_at_both_ends() {
        let mut editor = Editor::from_text("abc");
        assert_eq!(editor.caret(), 3);
        editor.move_caret(-2);
        assert_eq!(editor.caret(), 1);
        editor.move_caret(1);
        assert_eq!(editor.caret(), 2);
        // Held at either end rather than wrapping: a caret that jumped to the other end
        // would make correcting a typo a guess about where it landed.
        editor.move_caret(10);
        assert_eq!(editor.caret(), 3);
        editor.move_caret(-10);
        assert_eq!(editor.caret(), 0);
    }

    #[test]
    fn typing_in_the_middle_inserts_at_the_caret() {
        let mut editor = Editor::from_text("helo");
        editor.move_caret(-1);
        editor.insert("l");
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.caret(), 4, "the caret follows what was typed");
    }

    #[test]
    fn backspace_and_delete_remove_on_opposite_sides_of_the_caret() {
        // Backspace takes the character before the caret and moves it; Delete takes the one
        // under it and leaves it where it is. Swapping them silently edits the wrong
        // character, which is worse than not supporting the key at all.
        let mut editor = Editor::from_text("abc");
        editor.move_caret(-1);
        editor.backspace();
        assert_eq!(editor.text(), "ac");
        assert_eq!(editor.caret(), 1);
        editor.delete();
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.caret(), 1);
        // Neither one can run off either end.
        editor.delete();
        assert_eq!(editor.text(), "a");
        editor.backspace();
        assert_eq!(editor.text(), "");
        editor.backspace();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn the_kill_keys_act_around_the_caret() {
        let mut editor = Editor::from_text("one two three");
        editor.move_caret(-6);
        // Ctrl+W takes the word *before* the caret, wherever the caret is. The space that
        // separated it is not part of the word, so it stays — the same thing readline does.
        editor.delete_word();
        assert_eq!(editor.text(), "one  three");
        // Ctrl+U takes everything before it, Ctrl+K everything after.
        editor.delete_to_start();
        assert_eq!(editor.text(), " three");
        assert_eq!(editor.caret(), 0);
        editor.delete_to_end();
        assert_eq!(editor.text(), "");
        // Ctrl+W on an empty buffer is a no-op rather than a panic.
        editor.delete_word();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn ctrl_w_eats_the_spaces_before_the_word_too() {
        let mut editor = Editor::from_text("git commit ");
        editor.delete_word();
        assert_eq!(editor.text(), "git ");
        editor.delete_word();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn a_long_input_wraps_instead_of_scrolling_the_beginning_away() {
        // 12 columns, prompt 2 wide: the first row holds 10 cells, later rows the full 12.
        let text: String = ('a'..='z').collect();
        let rows = input_rows(&text, 12, 2);
        assert_eq!(rows[0].0, "› ");
        assert_eq!(rows[0].1, "abcdefghij");
        assert_eq!(
            rows[1].0, "  ",
            "continuation rows align with the text, not the prompt"
        );
        // Ten cells per row, because the two-column prefix is on every row.
        assert_eq!(rows[1].1, "klmnopqrst");
        assert_eq!(rows[2].1, "uvwxyz");
        // Nothing is lost: the beginning stays readable, which is the point.
        let rebuilt: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn a_short_input_stays_on_one_row() {
        let rows = input_rows("hello", 40, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "hello");
    }

    #[test]
    fn an_empty_input_still_has_a_row_for_the_caret() {
        let rows = input_rows("", 40, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "");
    }

    #[test]
    fn a_wide_character_never_splits_across_rows() {
        // 5 columns, prompt 2: three CJK cells fit on the first row, five on the rest.
        let rows = input_rows("你好世界你好世界", 5, 2);
        for (prefix, text) in &rows {
            let total = util::width(prefix) + util::width(text);
            assert!(total <= 5, "{prefix:?}{text:?} is {total} cells wide");
        }
        let rebuilt: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(rebuilt, "你好世界你好世界");
    }
}
