//! Fenced-code highlighting and table layout.
//!
//! Syntax highlighting is hand-written rather than pulled from a crate: the requirement is a
//! few common languages rendered well enough to read, and the markdown renderer is already a
//! scanner over lines, so a keyword table costs less than the dependency would.
//!
//! Streaming is why the scans are careful: a table that has not finished arriving is not a
//! table yet, and the renderer is handed whole rows only.

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


/// Languages where `#` always starts a comment (`python`, `ruby`, …) rather than only at a
/// word boundary (shell).
pub(super) fn hash_comments(lang: &str) -> bool {
    matches!(
        lang,
        "python" | "py" | "ruby" | "rb" | "perl" | "yaml" | "yml" | "toml" | "ini" | "makefile"
            | "dockerfile" | "graphql" | "r" | "julia" | "elixir" | "nim"
    )
}

pub(super) fn is_shell(lang: &str) -> bool {
    matches!(lang, "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "nu" | "ps1" | "powershell")
}

/// Languages this highlighter knows anything about.
///
/// An unknown language is not guessed at. Running a tokenizer over prose because a fence
/// said `text` colours random English words as keywords, which is the failure mode of
/// auto-detection and reads worse than no colour at all.
pub(super) fn is_code_like(lang: &str) -> bool {
    matches!(
        lang,
        "rust" | "rs" | "python" | "py" | "go" | "java" | "kotlin" | "kt" | "swift" | "c" | "h"
            | "cpp" | "cc" | "cxx" | "hpp" | "cs" | "csharp" | "js" | "javascript" | "ts"
            | "typescript" | "tsx" | "jsx" | "php" | "ruby" | "rb" | "lua" | "scala" | "dart"
            | "zig" | "sql" | "json" | "yaml" | "yml" | "toml" | "ini" | "diff" | "patch"
            | "makefile" | "dockerfile" | "graphql" | "proto"
    )
}

const KEYWORDS: &[&str] = &[
    // shell
    "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "in",
    "function", "return", "export", "local", "source", "set", "unset", "readonly", "shift",
    "trap", "exit", "test",
    // rust
    "fn", "let", "mut", "pub", "impl", "trait", "struct", "enum", "match", "use", "mod",
    "crate", "where", "async", "await", "move", "ref", "dyn", "loop",
    // python / ruby / lua
    "def", "class", "import", "from", "as", "with", "try", "except", "finally", "raise",
    "lambda", "pass", "yield", "and", "or", "not", "is", "elsif", "unless", "begin",
    "rescue", "end",
    // c family / go / js
    "void", "float", "double", "long", "short", "unsigned", "signed", "const", "static",
    "var", "func", "type", "interface", "package", "defer", "chan", "go", "map", "new",
    "delete", "this", "typeof", "instanceof", "extends", "implements", "public", "private",
    "protected", "final", "abstract", "override", "throws", "throw", "catch", "switch",
    "default", "break", "continue", "goto", "sizeof", "namespace", "using", "select",
    "insert", "update", "where", "join", "group", "order", "limit", "from", "values",
];

const LITERALS: &[&str] = &[
    "true", "false", "null", "nil", "None", "True", "False", "undefined", "self", "super",
];

const TYPES: &[&str] = &[
    "string", "bool", "boolean", "number", "str", "usize", "isize", "u8", "u16", "u32", "u64",
    "u128", "i8", "i16", "i32", "i64", "i128", "f32", "f64", "Option", "Result", "Vec",
    "String", "Box", "object", "dict", "list", "tuple", "bytes", "HashMap", "Error", "File",
    "Self", "int", "char", "byte", "any",
];

/// Colour one line of code.
///
/// Hand-written and deliberately shallow: a real tokenizer per language is a dependency the
/// project refuses (see agent.md §8), and the point of colour here is that a code block is
/// *scannable* — strings, comments, numbers and keywords are where the eye lands. Anything
/// unrecognised keeps the terminal's own foreground.
pub(super) fn highlight_code(text: &str, lang: Option<&str>) -> Vec<Span> {
    let lang = lang.unwrap_or("").to_lowercase();
    let shell = is_shell(&lang);
    if !is_code_like(&lang) && !shell {
        return vec![Span::new(text.to_string(), Style::plain())];
    }
    let hashes = hash_comments(&lang);
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        // Comments. In shell a `#` only starts one at a word boundary, so `${a#b}` stays
        // code; in python `#` is always a comment, and `x#y` is not valid python anyway.
        let line_comment = (hashes && chars[i] == '#')
            || (shell && chars[i] == '#' && (i == 0 || chars[i - 1].is_whitespace()))
            || (matches!(lang.as_str(), "sql" | "lua") && starts_with(&chars, i, &['-', '-']))
            || (!shell && starts_with(&chars, i, &['/', '/']));
        if line_comment {
            push(&mut spans, &mut buf, Style::plain());
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::new(rest, Style::new(Color::Dim)));
            break;
        }
        if !shell && starts_with(&chars, i, &['/', '*']) {
            let end = chars[i + 2..]
                .windows(2)
                .position(|w| w == ['*', '/'])
                .map(|offset| i + 2 + offset + 2)
                .unwrap_or(chars.len());
            push(&mut spans, &mut buf, Style::plain());
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::Dim)));
            i = end;
            continue;
        }
        // Strings, delimiter included: the quotes are how a reader sees where it ends.
        if matches!(chars[i], '"' | '\'' | '`') {
            let quote = chars[i];
            push(&mut spans, &mut buf, Style::plain());
            let mut end = i + 1;
            while end < chars.len() {
                if chars[end] == '\\' {
                    end += 2;
                    continue;
                }
                if chars[end] == quote {
                    end += 1;
                    break;
                }
                end += 1;
            }
            let end = end.min(chars.len());
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::SyntaxString)));
            i = end;
            continue;
        }
        // Numbers, including the 0x / 0b forms and a trailing unit.
        if chars[i].is_ascii_digit() && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
        {
            let mut end = i;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '.' | '_'))
            {
                // Stop before a `..` range, so `0..10` colour as `0` and `10`.
                if chars[end] == '.' && chars.get(end + 1) == Some(&'.') {
                    break;
                }
                end += 1;
            }
            push(&mut spans, &mut buf, Style::plain());
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::SyntaxNumber)));
            i = end;
            continue;
        }
        // Words: keywords, literals, types, and names being defined or called.
        if chars[i].is_alphabetic() || matches!(chars[i], '_' | '$') {
            let mut end = i;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || matches!(chars[end], '_' | '$'))
            {
                end += 1;
            }
            let word = &chars[i..end];
            let word_text: String = word.iter().collect();
            // A `--flag` is a keyword-class thing to someone reading a command line.
            let is_flag = shell && i > 0 && chars[i - 1] == '-';
            push(&mut spans, &mut buf, Style::plain());
            let style = if is_flag || KEYWORDS.contains(&word_text.as_str()) {
                Style::new(Color::SyntaxKeyword)
            } else if LITERALS.contains(&word_text.as_str()) || TYPES.contains(&word_text.as_str()) {
                // Literals and type names share a colour: both are values or shapes rather
                // than control flow, and two more hues would be two more things to ignore.
                Style::new(Color::SyntaxNumber)
            } else if chars[end..].iter().find(|c| **c != ' ').copied() == Some('(') {
                // A call or a definition: the name right before `(`.
                Style::new(Color::SyntaxKeyword)
            } else {
                Style::plain()
            };
            spans.push(Span::new(word_text, style));
            i = end;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    push(&mut spans, &mut buf, Style::plain());
    spans
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
        return rows
            .iter()
            .map(|row| Line::spans(inline(row.trim_end(), Style::plain())))
            .collect();
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
                let text = row
                    .iter()
                    .map(|cell| cell.trim())
                    .filter(|cell| !cell.is_empty())
                    .collect::<Vec<_>>()
                    .join("  ");
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
