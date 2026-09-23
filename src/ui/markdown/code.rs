//! Fenced-code highlighting and table layout.
//!
//! Rust, JavaScript, TypeScript, Bash, Kotlin, Python and JSON are highlighted by
//! tree-sitter (see [`super::syntax`]). The scanner below is what every other language
//! gets: a few token kinds, no grammar crate, and enough state to keep a comment or a
//! string coloured when it runs onto the next line. An unknown language is left plain —
//! guessing would colour English words as keywords.

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

/// Carry a block comment or a multi-line string from one fence line to the next.
///
/// Codex highlights a fence as one buffer, so a `/*` opened on line one still colours line
/// two. Doing each line in isolation drops that, and a comment then looks like code again
/// the moment it wraps.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Scan {
    block_comment: bool,
    /// Quote character of an open Python triple-quoted string.
    triple: Option<char>,
    /// Hash count of an open Rust raw string (`r#"…"#` is 1). `None` when not inside one.
    raw_hashes: Option<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Plain,
    Rust,
    Python,
    Go,
    C,
    Shell,
    Sql,
    Json,
    Data,
    Diff,
    Lua,
}

fn classify(lang: &str) -> Lang {
    match lang {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" | "ruby" | "rb" | "perl" | "makefile" | "dockerfile" | "graphql" | "proto"
        | "r" | "julia" | "elixir" | "nim" => Lang::Python,
        "go" => Lang::Go,
        "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "nu" | "ps1" | "powershell" => Lang::Shell,
        "sql" => Lang::Sql,
        "json" => Lang::Json,
        "yaml" | "yml" | "toml" | "ini" => Lang::Data,
        "diff" | "patch" => Lang::Diff,
        "lua" => Lang::Lua,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "cs" | "csharp" | "java" | "kotlin" | "kt" | "swift"
        | "js" | "javascript" | "ts" | "typescript" | "tsx" | "jsx" | "php" | "scala" | "dart" | "zig" => {
            Lang::C
        }
        _ => Lang::Plain,
    }
}

fn is_keyword(lang: Lang, word: &str) -> bool {
    if lang == Lang::Sql {
        return matches!(
            word.to_ascii_lowercase().as_str(),
            "select" | "from" | "where" | "and" | "or" | "not" | "insert" | "into" | "values"
                | "update" | "set" | "delete" | "join" | "left" | "right" | "inner" | "outer" | "on"
                | "group" | "order" | "by" | "limit" | "as" | "create" | "table" | "drop" | "alter"
                | "null" | "is" | "in" | "like" | "between" | "distinct" | "having" | "union"
        );
    }
    match lang {
        Lang::Rust => matches!(
            word,
            "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
                | "enum" | "extern" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match"
                | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self" | "static"
                | "struct" | "super" | "trait" | "type" | "unsafe" | "use" | "where" | "while"
        ),
        Lang::Python => matches!(
            word,
            "and" | "as" | "assert" | "async" | "await" | "begin" | "break" | "case" | "class"
                | "continue" | "def" | "del" | "do" | "elif" | "else" | "end" | "esac" | "except"
                | "fi" | "finally" | "for" | "from" | "function" | "global" | "if" | "import" | "in"
                | "is" | "lambda" | "nonlocal" | "not" | "or" | "pass" | "raise" | "return" | "then"
                | "try" | "while" | "with" | "yield"
        ),
        Lang::Go => matches!(
            word,
            "break" | "case" | "chan" | "const" | "continue" | "default" | "defer" | "else"
                | "fallthrough" | "for" | "func" | "go" | "goto" | "if" | "import" | "interface"
                | "map" | "package" | "range" | "return" | "select" | "struct" | "switch" | "type"
                | "var"
        ),
        Lang::C => matches!(
            word,
            "async" | "await" | "break" | "case" | "catch" | "class" | "const" | "continue"
                | "default" | "delete" | "do" | "else" | "enum" | "export" | "extends" | "finally"
                | "for" | "function" | "if" | "implements" | "import" | "in" | "instanceof"
                | "interface" | "let" | "namespace" | "new" | "of" | "override" | "package"
                | "private" | "protected" | "public" | "return" | "static" | "struct" | "switch"
                | "this" | "throw" | "try" | "typeof" | "using" | "var" | "virtual" | "void"
                | "while" | "yield"
        ),
        Lang::Shell => matches!(
            word,
            "if" | "then" | "else" | "elif" | "fi" | "for" | "while" | "do" | "done" | "case"
                | "esac" | "in" | "function" | "return" | "export" | "local" | "source" | "set"
                | "unset" | "readonly" | "shift" | "trap" | "exit"
        ),
        Lang::Lua => matches!(
            word,
            "and" | "break" | "do" | "else" | "elseif" | "end" | "false" | "for" | "function"
                | "goto" | "if" | "in" | "local" | "nil" | "not" | "or" | "repeat" | "return"
                | "then" | "true" | "until" | "while"
        ),
        Lang::Sql | Lang::Json | Lang::Data | Lang::Diff | Lang::Plain => false,
    }
}

fn is_literal(word: &str) -> bool {
    matches!(
        word,
        "true" | "false" | "null" | "nil" | "None" | "True" | "False" | "undefined"
    )
}

fn is_type_name(word: &str) -> bool {
    matches!(
        word,
        "string" | "bool" | "boolean" | "number" | "str" | "usize" | "isize" | "u8" | "u16" | "u32"
            | "u64" | "u128" | "i8" | "i16" | "i32" | "i64" | "i128" | "f32" | "f64" | "Option"
            | "Result" | "Vec" | "String" | "Box" | "HashMap" | "int" | "char" | "byte" | "any"
            | "uint" | "int32" | "int64" | "float64" | "error"
    ) || looks_like_type(word)
}

/// `UserId` is a type; `user_id` and `URL` are not. Codex's grammars know this from the
/// syntax; a capital followed by a lower-case letter is the shape those grammars colour.
fn looks_like_type(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else { return false };
    first.is_uppercase()
        && chars.any(|c| c.is_lowercase())
        && word.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Colour a whole fence.
///
/// Rust, JavaScript, TypeScript, Bash, Kotlin, Python and JSON go through tree-sitter, one
/// parse for the block, so a comment that crosses a line stays a comment. Anything else — or
/// a block the grammar refuses — uses the hand-written scanner, which carries its own state
/// line to line.
pub(super) fn highlight_fence(lang: Option<&str>, lines: &[String]) -> Vec<Vec<Span>> {
    if let Some(painted) = super::syntax::paint(lang, lines)
        && painted.len() == lines.len()
    {
        return painted;
    }
    let mut scan = Scan::default();
    lines.iter().map(|line| highlight_code(line, lang, &mut scan)).collect()
}

/// Colour one line of a fence, continuing `scan` from the line above.
pub(super) fn highlight_code(text: &str, lang: Option<&str>, scan: &mut Scan) -> Vec<Span> {
    let lang_name = lang.unwrap_or("").to_ascii_lowercase();
    let lang = classify(&lang_name);
    if lang == Lang::Plain {
        return vec![Span::new(text.to_string(), Style::plain())];
    }
    if lang == Lang::Diff {
        return vec![Span::new(text.to_string(), diff_style(text))];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    if let Some((end, color)) = resume(scan, &chars) {
        if end > 0 {
            spans.push(Span::new(chars[..end].iter().collect::<String>(), Style::new(color)));
        }
        if end >= chars.len() {
            return spans;
        }
        i = end;
    }

    while i < chars.len() {
        if line_comment_at(&chars, i, lang) {
            push(&mut spans, &mut buf, Style::plain());
            spans.push(Span::new(chars[i..].iter().collect::<String>(), Style::new(Color::SyntaxComment)));
            break;
        }
        if matches!(lang, Lang::Rust | Lang::Go | Lang::C | Lang::Sql) && starts_with(&chars, i, &['/', '*']) {
            push(&mut spans, &mut buf, Style::plain());
            match block_comment_end(&chars, i + 2) {
                Some(end) => {
                    spans.push(Span::new(
                        chars[i..end].iter().collect::<String>(),
                        Style::new(Color::SyntaxComment),
                    ));
                    i = end;
                }
                None => {
                    spans.push(Span::new(chars[i..].iter().collect::<String>(), Style::new(Color::SyntaxComment)));
                    scan.block_comment = true;
                    break;
                }
            }
            continue;
        }
        if lang == Lang::Rust && chars[i] == '#' && chars.get(i + 1) == Some(&'[') {
            push(&mut spans, &mut buf, Style::plain());
            let end = bracket_end(&chars, i + 1);
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::SyntaxType)));
            i = end;
            continue;
        }
        if lang == Lang::Rust && chars[i] == '\'' && char_literal_end(&chars, i).is_none() && is_lifetime(&chars, i)
        {
            push(&mut spans, &mut buf, Style::plain());
            let end = word_end(&chars, i + 1);
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::SyntaxType)));
            i = end;
            continue;
        }
        if let Some(end) = string_at(&chars, i, lang, scan) {
            push(&mut spans, &mut buf, Style::plain());
            let mut color = Color::SyntaxString;
            if lang == Lang::Json && next_non_space(&chars, end) == Some(':') {
                color = Color::SyntaxFunction;
            }
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(color)));
            if end >= chars.len() {
                break;
            }
            i = end;
            continue;
        }
        if is_number_start(&chars, i) {
            let end = number_end(&chars, i);
            push(&mut spans, &mut buf, Style::plain());
            spans.push(Span::new(chars[i..end].iter().collect::<String>(), Style::new(Color::SyntaxNumber)));
            i = end;
            continue;
        }
        if is_word_start(chars[i]) {
            let end = word_end(&chars, i);
            let word: String = chars[i..end].iter().collect();
            push(&mut spans, &mut buf, Style::plain());
            spans.push(Span::new(word.clone(), word_style(lang, &word, &chars, i, end)));
            i = end;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    push(&mut spans, &mut buf, Style::plain());
    if spans.is_empty() {
        spans.push(Span::plain(String::new()));
    }
    spans
}

fn diff_style(text: &str) -> Style {
    if text.starts_with('+') && !text.starts_with("+++") {
        Style::new(Color::DiffAddedText)
    } else if text.starts_with('-') && !text.starts_with("---") {
        Style::new(Color::DiffRemovedText)
    } else if text.starts_with("@@") || text.starts_with("diff ") || text.starts_with("+++") || text.starts_with("---")
    {
        Style::new(Color::Cyan)
    } else {
        Style::plain()
    }
}

/// How far a region carried from the previous line extends, and what colour it keeps.
fn resume(scan: &mut Scan, chars: &[char]) -> Option<(usize, Color)> {
    if scan.block_comment {
        return Some(match block_comment_end(chars, 0) {
            Some(end) => {
                scan.block_comment = false;
                (end, Color::SyntaxComment)
            }
            None => (chars.len(), Color::SyntaxComment),
        });
    }
    if let Some(quote) = scan.triple {
        return Some(match triple_end(chars, 0, quote) {
            Some(end) => {
                scan.triple = None;
                (end, Color::SyntaxString)
            }
            None => (chars.len(), Color::SyntaxString),
        });
    }
    if let Some(hashes) = scan.raw_hashes {
        return Some(match raw_close(chars, 0, hashes) {
            Some(end) => {
                scan.raw_hashes = None;
                (end, Color::SyntaxString)
            }
            None => (chars.len(), Color::SyntaxString),
        });
    }
    None
}

fn line_comment_at(chars: &[char], i: usize, lang: Lang) -> bool {
    let hash = chars[i] == '#'
        && matches!(lang, Lang::Python | Lang::Shell | Lang::Data)
        && (lang != Lang::Shell || i == 0 || chars[i - 1].is_whitespace());
    let slashes = matches!(lang, Lang::Rust | Lang::Go | Lang::C) && starts_with(chars, i, &['/', '/']);
    let dashes = matches!(lang, Lang::Sql | Lang::Lua) && starts_with(chars, i, &['-', '-']);
    hash || slashes || dashes
}

fn block_comment_end(chars: &[char], from: usize) -> Option<usize> {
    chars[from..].windows(2).position(|pair| pair == ['*', '/']).map(|offset| from + offset + 2)
}

fn bracket_end(chars: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

fn string_at(chars: &[char], i: usize, lang: Lang, scan: &mut Scan) -> Option<usize> {
    if lang == Lang::Rust
        && let Some((end, hashes, closed)) = rust_raw_at(chars, i)
    {
        if !closed {
            scan.raw_hashes = Some(hashes);
        }
        return Some(end);
    }
    if lang == Lang::Python && (starts_with(chars, i, &['"', '"', '"']) || starts_with(chars, i, &['\'', '\'', '\'']))
    {
        let quote = chars[i];
        return Some(match triple_end(chars, i + 3, quote) {
            Some(end) => end,
            None => {
                scan.triple = Some(quote);
                chars.len()
            }
        });
    }
    if matches!(chars.get(i), Some('"' | '\'' | '`')) {
        Some(quoted_end(chars, i))
    } else {
        None
    }
}

fn rust_raw_at(chars: &[char], i: usize) -> Option<(usize, u8, bool)> {
    let mut j = i;
    if matches!(chars.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0u8;
    while chars.get(j) == Some(&'#') {
        hashes = hashes.saturating_add(1);
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    match raw_close(chars, j, hashes) {
        Some(end) => Some((end, hashes, true)),
        None => Some((chars.len(), hashes, false)),
    }
}

fn raw_close(chars: &[char], from: usize, hashes: u8) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut matched = 0u8;
            while matched < hashes && chars.get(i + 1 + matched as usize) == Some(&'#') {
                matched += 1;
            }
            if matched == hashes {
                return Some(i + 1 + hashes as usize);
            }
        }
        i += 1;
    }
    None
}

fn triple_end(chars: &[char], from: usize, quote: char) -> Option<usize> {
    let mut i = from;
    while i + 2 < chars.len() {
        if chars[i] == quote && chars[i + 1] == quote && chars[i + 2] == quote {
            return Some(i + 3);
        }
        i += 1;
    }
    None
}

fn char_literal_end(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'\'') {
        return None;
    }
    if chars.get(i + 1) == Some(&'\\') {
        return (chars.get(i + 3) == Some(&'\'')).then_some(i + 4);
    }
    (chars.get(i + 2) == Some(&'\'')).then_some(i + 3)
}

fn is_lifetime(chars: &[char], i: usize) -> bool {
    chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
}

fn quoted_end(chars: &[char], i: usize) -> usize {
    let quote = chars[i];
    let mut end = i + 1;
    while end < chars.len() {
        if chars[end] == '\\' {
            end += 2;
            continue;
        }
        if chars[end] == quote {
            return end + 1;
        }
        end += 1;
    }
    chars.len()
}

fn is_number_start(chars: &[char], i: usize) -> bool {
    chars[i].is_ascii_digit() && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
}

fn number_end(chars: &[char], i: usize) -> usize {
    let mut end = i;
    while end < chars.len() && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '.' | '_')) {
        if chars[end] == '.' && chars.get(end + 1) == Some(&'.') {
            break;
        }
        end += 1;
    }
    end
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn word_end(chars: &[char], i: usize) -> usize {
    let mut end = i + 1;
    while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '$') {
        end += 1;
    }
    end
}

fn next_non_space(chars: &[char], i: usize) -> Option<char> {
    chars[i..].iter().copied().find(|c| !c.is_whitespace())
}

fn word_style(lang: Lang, word: &str, chars: &[char], start: usize, end: usize) -> Style {
    let flag = lang == Lang::Shell && start > 0 && chars[start - 1] == '-';
    let shell_var = lang == Lang::Shell && word.starts_with('$');
    let key = lang == Lang::Data && next_non_space(chars, end) == Some(':');
    let call = next_non_space(chars, end).is_some_and(|c| c == '(' || c == '!');
    let color = if flag || is_keyword(lang, word) {
        Color::SyntaxKeyword
    } else if is_literal(word) {
        Color::SyntaxNumber
    } else if shell_var || call || key {
        Color::SyntaxFunction
    } else if is_type_name(word) {
        Color::SyntaxType
    } else {
        Color::Text
    };
    Style::new(color)
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
