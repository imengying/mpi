//! A small markdown renderer for assistant text.
//!
//! The model writes markdown; showing the marks (`**`, fences, `#`, `|`) as characters is
//! what makes an answer look like a raw log. This turns the common forms into spans the
//! screen already knows how to paint, and drops the punctuation that only told a machine
//! what to do.
//!
//! It is not a CommonMark implementation and does not pretend to be: no footnotes, no raw
//! HTML, no reference links. What it covers is what models actually emit — headings,
//! emphasis, inline code, fenced code with shell/JSON highlighting, lists (nested, ordered,
//! task), tables, quotes, rules and links.
//!
//! Three rules shape every decision here:
//!
//! * **An unclosed mark stays literal.** A stream is rendered on every token, so eating a
//!   `**` that has not closed yet would make the preview flicker between styled and plain as
//!   the rest of the word arrives. Half a table is not a table either: it renders as text
//!   until the `---` separator row shows up.
//! * **Marks that only carry meaning are dropped; marks a reader needs are kept.** `**`
//!   disappears and the text goes bold; `- ` stays, because it is how a list reads; `>`
//!   becomes `│ `; a fence becomes a labelled bar. Nothing is added — no "1 of 3", no
//!   "table starts here".
//! * **Nothing interprets its own contents twice.** Text inside a fence, and inside inline
//!   code, is never scanned for emphasis or links.
//!
//! **Wrapping is not done here.** Every consumer already wraps what it is given, and a
//! renderer that wrapped its own output would be wrapped a second time — a second pass that
//! cannot tell an indent it should keep from one it has already applied. Instead each line
//! carries the indent its continuation rows want in [`Line::hang`], and the single wrap that
//! happens later produces the right shape at whatever width it is given.

use crate::ui::screen::{Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

/// How wide a table may get before the border is dropped.
///
/// A bordered table that has to wrap is worse than a plain one: the columns stop lining up
/// exactly when the alignment was the reason for the border. Past this the rows are laid
/// out as plain text with the pipes removed.
const TABLE_MAX_WIDTH: usize = 100;

/// Indent for a fenced block's body, so code is visibly set in from the prose.
const CODE_INDENT: &str = "  ";

/// Render `text` to lines, **unwrapped**.
///
/// Wrapping is left to [`crate::ui::screen::wrap_all`], which every consumer already calls:
/// a markdown function that wrapped its own output would be wrapped a second time by the
/// screen, and the second pass cannot tell an indent it should keep from one it has already
/// applied. Instead each line carries the indent its continuation rows want in
/// [`Line::hang`], and one wrap — wherever it happens — produces the right shape.
///
/// `width` is therefore used for one thing: deciding whether a table fits. A table has to
/// commit to column widths while it is being built, and a table that is too wide for the
/// terminal is laid out as plain rows instead.
pub fn render(text: &str, width: usize) -> Vec<Line> {
    // ANSI in the source is stripped: the styles below are the only colour this text may
    // carry, and a model that emits escapes must not be able to paint the terminal.
    let clean = util::sanitize(text);
    let mut lines: Vec<Line> = Vec::new();
    // A fence's body is buffered so its frame can be sized to the widest line. An open
    // fence is `Some`, and the body is flushed when the closing marker arrives (or at the
    // end of the text, since the tail of a stream is an unterminated fence).
    let mut fence: Option<(Fence, Vec<Line>)> = None;
    let mut table: Vec<String> = Vec::new();

    for raw in clean.split('\n') {
        // Inside a fence everything is code: no headings, no emphasis, no tables.
        if let Some((open, body)) = &mut fence {
            if closes(open, raw) {
                if let Some((open, body)) = fence.take() {
                    lines.extend(open.frame(&body, width));
                }
            } else {
                body.push(code_line(raw.trim_end(), open.lang.as_deref()));
            }
            continue;
        }
        if let Some(open) = Fence::open(raw) {
            flush_table(&mut lines, &mut table, width);
            fence = Some((open, Vec::new()));
            continue;
        }
        // A `|`-row is only a table once the separator confirms it, so the rows are
        // collected and rendered together.
        if looks_like_table_row(raw) {
            table.push(raw.to_string());
            continue;
        }
        flush_table(&mut lines, &mut table, width);
        lines.extend(block_line(raw.trim_end()));
    }
    // An unterminated fence is the tail of a stream: the closing marker has not arrived.
    // The block is still drawn, because the alternative is showing nothing and then having
    // a whole code block appear at once when the fence closes.
    if let Some((open, body)) = fence.take() {
        lines.extend(open.frame(&body, width));
    }
    flush_table(&mut lines, &mut table, width);

    // Trailing blank lines are dropped: the transcript adds its own spacing, and a run of
    // them at the end reads as a gap the answer did not ask for.
    while lines.last().is_some_and(is_blank_line) {
        lines.pop();
    }
    // Runs of blank lines collapse to one: models use blank lines as punctuation, and three
    // of them in a row are three times the pause.
    let mut out: Vec<Line> = Vec::with_capacity(lines.len());
    let mut pending_blank = false;
    for line in lines {
        if is_blank_line(&line) {
            pending_blank = true;
            continue;
        }
        if pending_blank && !out.is_empty() {
            out.push(Line::blank());
        }
        pending_blank = false;
        out.push(line);
    }
    out
}

fn is_blank_line(line: &Line) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

/// One line of a fenced block. The indent is part of the line, and `hang` matches it so a
/// wrapped code line lines up under the code rather than under the frame.
fn code_line(text: &str, lang: Option<&str>) -> Line {
    let mut spans = vec![Span::new(CODE_INDENT, Style::plain())];
    spans.extend(highlight_code(text, lang));
    Line::hanging(spans, CODE_INDENT.len())
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// A fence that is open, and what its info string said.
#[derive(Debug)]
struct Fence {
    lang: Option<String>,
    marker: char,
    /// The opening marker's length: a ```` ```` ```` fence is not closed by ```` ``` ````.
    marker_len: usize,
}

impl Fence {
    fn open(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next()?;
        if marker != '`' && marker != '~' {
            return None;
        }
        let marker_len = trimmed.chars().take_while(|c| *c == marker).count();
        if marker_len < 3 {
            return None;
        }
        // The info string is a language in practice. The first word is taken so
        // `rust,ignore` and `sh title=x` still name one.
        let info = trimmed[marker_len..].trim();
        let lang = info
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches(',')
            .to_string();
        Some(Fence {
            lang: (!lang.is_empty()).then_some(lang),
            marker,
            marker_len,
        })
    }

    /// The whole block: an opening bar, the code, and a closing bar of the same width.
    ///
    /// A fence's backticks are punctuation for a parser, and leaving them on screen makes a
    /// code block look like a log line. The bar carries what a reader needs from that line —
    /// that this is code, and in which language — and nothing else.
    ///
    /// Both bars are the same length on purpose: the pair reads as a frame around the code,
    /// and a frame that does not close is worse than no frame. Its length comes from the
    /// longest line, so a block of short commands is not wrapped in sixty columns of rule.
    fn frame(&self, body: &[Line], width: usize) -> Vec<Line> {
        let longest = body.iter().map(Line::width).max().unwrap_or(0);
        let label = self.label();
        let label_width = label.as_ref().map_or(0, |label| util::width(label));
        // The corner, the label, and a little air past the widest line.
        let wanted = longest.max(label_width) + 3;
        let bar_width = wanted.min(width.saturating_sub(1)).max(2);
        let mut out = Vec::with_capacity(body.len() + 2);
        out.push(self.bar('┌', bar_width, label.as_deref()));
        out.extend(body.iter().cloned());
        // The language is named once, on the bar that opens the block: repeating it on the
        // way out adds nothing a reader is still looking for by then.
        out.push(self.bar('└', bar_width, None));
        out
    }

    /// `─ lang ` when the fence named one, `None` otherwise.
    fn label(&self) -> Option<String> {
        self.lang.as_ref().map(|lang| format!("─ {lang} "))
    }

    /// One bar of exactly `total` columns, with `label` after the corner when given.
    fn bar(&self, corner: char, total: usize, label: Option<&str>) -> Line {
        let label = util::truncate(label.unwrap_or(""), total.saturating_sub(1), "");
        let rule = "─".repeat(total.saturating_sub(1 + util::width(&label)));
        Line::spans(vec![
            Span::new(corner.to_string(), Style::new(Color::Dim)),
            Span::new(label, Style::new(Color::Dim)),
            Span::new(rule, Style::new(Color::Dim)),
        ])
    }
}

/// Whether `line` closes the open fence: same marker, at least as long, nothing else on it.
fn closes(open: &Fence, line: &str) -> bool {
    let trimmed = line.trim();
    let count = trimmed.chars().take_while(|c| *c == open.marker).count();
    count >= open.marker_len && trimmed[count..].trim().is_empty()
}

fn looks_like_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn flush_table(lines: &mut Vec<Line>, table: &mut Vec<String>, width: usize) {
    if table.is_empty() {
        return;
    }
    let collected = std::mem::take(table);
    lines.extend(render_table(&collected, width));
}

fn block_line(line: &str) -> Vec<Line> {
    if line.trim().is_empty() {
        return vec![Line::blank()];
    }
    if is_rule(line) {
        return vec![Line::new("─".repeat(24), Style::new(Color::Dim))];
    }
    if let Some(rest) = heading(line) {
        return vec![Line::spans(inline(rest, Style::bold(Color::Cyan)))];
    }
    // Quotes nest: `>> x` and `> > x` are both two levels deep, and the number of bars is
    // how a reader sees the nesting without counting characters.
    let (level, body) = quote_depth(line);
    if level > 0 {
        let mut spans: Vec<Span> = Vec::new();
        for _ in 0..level {
            spans.push(Span::new("│\u{a0}", Style::new(Color::Dim)));
        }
        spans.extend(inline(body, Style::new(Color::Dim)));
        return vec![Line::hanging(spans, level * 2)];
    }
    if let Some((marker, body)) = list_item(line) {
        let hang = util::width(&marker);
        // The marker is glued to the first word with a non-breaking space, so a narrow
        // terminal cannot leave a bullet alone on a row with its text underneath — which is
        // what a plain space does, and it reads as a lost item.
        // The marker's own trailing space becomes the non-breaking one, so the marker and
        // the first word are one unbreakable run without changing the visible spacing.
        let glued = format!("{}\u{a0}", marker.trim_end());
        let mut spans = vec![Span::new(glued, Style::new(Color::Cyan))];
        spans.extend(inline(body, Style::plain()));
        return vec![Line::hanging(spans, hang)];
    }
    vec![Line::spans(inline(line, Style::plain()))]
}

/// `>`, `>>`, `> >` → the depth and the text after the markers.
fn quote_depth(line: &str) -> (usize, &str) {
    let mut rest = line.trim_start();
    let mut level = 0;
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.strip_prefix(' ').unwrap_or(after);
        level += 1;
        // Bounded so a line of `>`s cannot spin here.
        if level >= 32 {
            break;
        }
    }
    (level, rest)
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    // Separated markers count: `- - -` and `* * *` are rules in every dialect.
    let bare: String = trimmed.chars().filter(|c| *c != ' ').collect();
    bare.len() >= 3
        && bare
            .chars()
            .next()
            .is_some_and(|first| matches!(first, '-' | '*' | '_') && bare.chars().all(|c| c == first))
}

fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
        return Some(line[hashes + 1..].trim_end());
    }
    None
}

/// `"- item"` / `"1. item"` / `"- [x] done"` → the marker to draw and the body.
///
/// The indent is kept in the marker so nesting survives, and an ordered list keeps its
/// numbers: replacing them with bullets would drop the thing the writer chose to say.
fn list_item(line: &str) -> Option<(String, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    // Markdown nests by two spaces or a tab. Deeper levels are clamped so a pathological
    // indent cannot push the text off the right edge.
    let depth = (indent.replace('\t', "  ").len() / 2).min(4);
    let pad = "  ".repeat(depth);
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        let body = &rest[2..];
        let (box_mark, body) = task_marker(body);
        let marker = match box_mark {
            Some(mark) => format!("{pad}{mark} "),
            None => format!("{pad}• "),
        };
        return Some((marker, body));
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits < 6 {
        let after = &rest[digits..];
        if let Some(body) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some((format!("{pad}{}. ", &rest[..digits]), body));
        }
    }
    None
}

/// `"[ ] x"` / `"[x] x"` → the box to draw and the text after it.
fn task_marker(body: &str) -> (Option<&'static str>, &str) {
    if let Some(rest) = body.strip_prefix("[ ] ") {
        return (Some("☐"), rest);
    }
    if let Some(rest) = body.strip_prefix("[x] ").or_else(|| body.strip_prefix("[X] ")) {
        return (Some("☑"), rest);
    }
    (None, body)
}

// ---------------------------------------------------------------------------
// Inline
// ---------------------------------------------------------------------------

fn inline(text: &str, style: Style) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        // `code` first, so marks inside it stay literal.
        if chars[i] == '`'
            && let Some(end) = chars[i + 1..].iter().position(|c| *c == '`')
        {
            push(&mut spans, &mut buf, style);
            let code: String = chars[i + 1..i + 1 + end].iter().collect();
            if !code.is_empty() {
                spans.push(Span::new(code, Style::new(Color::Green)));
            }
            i += end + 2;
            continue;
        }
        // Images before links: `![alt](url)` starts with `[` one character in.
        if chars[i] == '!'
            && chars.get(i + 1) == Some(&'[')
            && let Some((alt, _, next)) = link_at(&chars, i + 1)
        {
            push(&mut spans, &mut buf, style);
            spans.push(Span::new("🖼 ", Style::new(Color::Dim)));
            if !alt.is_empty() {
                spans.push(Span::new(alt, Style::new(Color::Dim)));
            }
            i = next;
            continue;
        }
        if chars[i] == '['
            && let Some((label, url, next)) = link_at(&chars, i)
        {
            push(&mut spans, &mut buf, style);
            if !label.is_empty() {
                spans.push(Span::new(
                    label.clone(),
                    Style { fg: Color::Cyan, underline: true, ..style },
                ));
            }
            // The URL is shown when it says something the label does not; when the label
            // *is* the URL, there is nothing to add.
            if !url.is_empty() && url != label {
                spans.push(Span::plain(" "));
                spans.push(Span::new(format!("({url})"), Style::new(Color::Dim)));
            }
            i = next;
            continue;
        }
        if let Some((url, next)) = bare_url(&chars, i) {
            push(&mut spans, &mut buf, style);
            spans.push(Span::new(url, Style { underline: true, ..style }));
            i = next;
            continue;
        }
        if let Some((marker, end)) = closer(&chars, i) {
            push(&mut spans, &mut buf, style);
            let inner: String = chars[i + marker.len()..end].iter().collect();
            let inner_style = match marker {
                "**" | "__" => Style { bold: true, ..style },
                "*" | "_" => Style { italic: true, ..style },
                // Strikethrough has no terminal attribute; dim is what "crossed out" reads
                // as once colour is all you have.
                "~~" => Style { fg: Color::Dim, ..style },
                // `==highlight==` has nowhere to go in a terminal palette either.
                "==" => Style { underline: true, ..style },
                _ => style,
            };
            // Inner marks are rendered too, so `**bold with \`code\`**` keeps both.
            spans.extend(inline(&inner, inner_style));
            i = end + marker.len();
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    push(&mut spans, &mut buf, style);
    if spans.is_empty() {
        spans.push(Span::plain(String::new()));
    }
    spans
}

fn push(spans: &mut Vec<Span>, buf: &mut String, style: Style) {
    if buf.is_empty() {
        return;
    }
    spans.push(Span::new(std::mem::take(buf), style));
}

/// A `**` / `__` / `*` / `_` / `~~` / `==` that has a matching closer, and where it starts.
fn closer(chars: &[char], at: usize) -> Option<(&'static str, usize)> {
    let marker = if starts_with(chars, at, &['*', '*']) {
        "**"
    } else if starts_with(chars, at, &['_', '_']) {
        "__"
    } else if starts_with(chars, at, &['~', '~']) {
        "~~"
    } else if starts_with(chars, at, &['=', '=']) {
        "=="
    } else if chars[at] == '*' {
        "*"
    } else if chars[at] == '_' && word_boundary(chars, at) {
        "_"
    } else {
        return None;
    };
    let mark: Vec<char> = marker.chars().collect();
    let mut j = at + mark.len();
    while j + mark.len() <= chars.len() {
        if starts_with(chars, j, &mark) {
            // `snake_case` and `__init__`: an underscore inside a word is not emphasis.
            if marker == "_" && j < chars.len() && chars.get(j + 1).is_some_and(|c| c.is_ascii_alphanumeric()) {
                j += 1;
                continue;
            }
            // An empty pair (`****`) is not emphasis, it is four asterisks.
            if j == at + mark.len() {
                return None;
            }
            return Some((marker, j));
        }
        j += 1;
    }
    None
}

fn word_boundary(chars: &[char], at: usize) -> bool {
    at == 0 || !chars[at - 1].is_ascii_alphanumeric()
}

fn starts_with(chars: &[char], at: usize, mark: &[char]) -> bool {
    chars.get(at..at + mark.len()).is_some_and(|got| got == mark)
}

/// `[label](url)` starting at `at`. Returns the label, the url, and the index after the link.
fn link_at(chars: &[char], at: usize) -> Option<(String, String, usize)> {
    let close = chars[at + 1..].iter().position(|c| *c == ']')?;
    let label_end = at + 1 + close;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let url_end = chars[label_end + 2..].iter().position(|c| *c == ')')?;
    let url_at = label_end + 2 + url_end;
    let label: String = chars[at + 1..label_end].iter().collect();
    let url: String = chars[label_end + 2..url_at].iter().collect();
    Some((label, url, url_at + 1))
}

/// A bare `http(s)://…` / `www.…` starting at `at`, and the index after it.
///
/// Trailing punctuation is excluded: in "see https://x.dev." the full stop belongs to the
/// sentence, and underlining it makes the link look like it has a typo.
fn bare_url(chars: &[char], at: usize) -> Option<(String, usize)> {
    if !starts_with(chars, at, &['h', 't', 't', 'p']) && !starts_with(chars, at, &['w', 'w', 'w']) {
        return None;
    }
    // Only at a word boundary, or a `xhttps://` would be highlighted from its `h`.
    if at > 0 && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '/') {
        return None;
    }
    let mut end = at;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    while end > at
        && matches!(chars[end - 1], '.' | ',' | ';' | '!' | '?' | ')' | ']' | '"' | '\'')
    {
        end -= 1;
    }
    if end.saturating_sub(at) < 5 || !chars[at..end].contains(&'.') {
        return None;
    }
    Some((chars[at..end].iter().collect(), end))
}

// ---------------------------------------------------------------------------
// Code
// ---------------------------------------------------------------------------

/// Languages where `#` always starts a comment (`python`, `ruby`, …) rather than only at a
/// word boundary (shell).
fn hash_comments(lang: &str) -> bool {
    matches!(
        lang,
        "python" | "py" | "ruby" | "rb" | "perl" | "yaml" | "yml" | "toml" | "ini" | "makefile"
            | "dockerfile" | "graphql" | "r" | "julia" | "elixir" | "nim"
    )
}

fn is_shell(lang: &str) -> bool {
    matches!(lang, "sh" | "bash" | "zsh" | "shell" | "console" | "fish" | "nu" | "ps1" | "powershell")
}

/// Languages this highlighter knows anything about.
///
/// An unknown language is not guessed at. Running a tokenizer over prose because a fence
/// said `text` colours random English words as keywords, which is the failure mode of
/// auto-detection and reads worse than no colour at all.
fn is_code_like(lang: &str) -> bool {
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
fn highlight_code(text: &str, lang: Option<&str>) -> Vec<Span> {
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

fn render_table(rows: &[String], width: usize) -> Vec<Line> {
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

#[cfg(test)]
mod tests {
    use super::*;

    

    fn rendered(input: &str) -> Vec<String> {
        render(input, 80).iter().map(visible).collect()
    }

    fn find<'a>(rows: &'a [Line], needle: &str) -> &'a Line {
        rows.iter()
            .find(|row| row.text().contains(needle))
            .unwrap_or_else(|| panic!("no line contains {needle:?}: {:?}", rendered_rows(rows)))
    }

    /// The span whose text *is* `needle`. Positional indexing is not used in these tests:
    /// it silently binds a test to how many runs the renderer happens to emit.
    fn span<'a>(rows: &'a [Line], needle: &str) -> &'a Span {
        rows.iter()
            .flat_map(|row| row.spans.iter())
            .find(|span| span.text == needle)
            .unwrap_or_else(|| panic!("no span is {needle:?}: {:?}", rendered_rows(rows)))
    }

    fn rendered_rows(rows: &[Line]) -> Vec<String> {
        rows.iter().map(visible).collect()
    }

    /// The text as the terminal receives it: a non-breaking space is a wrapping hint, and
    /// the screen paints it as an ordinary space.
    fn visible(line: &Line) -> String {
        line.text().replace('\u{a0}', " ")
    }

    #[test]
    fn plain_prose_is_unchanged() {
        assert_eq!(rendered("hello\nworld"), vec!["hello", "world"]);
        assert_eq!(render("hello", 80)[0].spans[0].style, Style::plain());
    }

    #[test]
    fn emphasis_code_and_links_lose_their_marks() {
        let rows = render("see **bold** and `code` plus [docs](https://example.com)", 80);
        assert_eq!(rows[0].text(), "see bold and code plus docs (https://example.com)");
        assert!(span(&rows, "bold").style.bold);
        assert_eq!(span(&rows, "code").style.fg, Color::Green);
        assert!(span(&rows, "docs").style.underline, "a link is underlined");
        assert_eq!(span(&rows, "docs").style.fg, Color::Cyan);
    }

    #[test]
    fn italics_are_italics_not_grey() {
        // The old renderer painted `*this*` dim, which reads as a hint rather than as
        // emphasis: it is the same colour the program uses for things the eye should skip.
        let rows = render("an *emphasis* here", 80);
        let emphasis = span(&rows, "emphasis");
        assert!(emphasis.style.italic, "{emphasis:?}");
        assert_ne!(emphasis.style.fg, Color::Dim, "emphasis must not be painted as a hint");
    }

    #[test]
    fn an_unclosed_mark_stays_literal() {
        // A stream arrives one token at a time: eating half a `**` would flicker.
        assert_eq!(rendered("still **open"), vec!["still **open"]);
        assert_eq!(rendered("and *this"), vec!["and *this"]);
        assert_eq!(rendered("`half"), vec!["`half"]);
    }

    #[test]
    fn code_marks_are_not_interpreted_inside_a_fence() {
        let rows = render("```rs\nlet x = **no**;\n```", 80);
        // The body is one row, and the `**` is still there: inside a fence nothing is
        // scanned for emphasis.
        assert_eq!(rendered_rows(&rows)[1], "  let x = **no**;");
        assert_eq!(span(&rows, "let").style.fg, Color::SyntaxKeyword, "`let` is a keyword");
    }

    #[test]
    fn a_fence_becomes_a_labelled_bar_without_its_markers() {
        let text = rendered("before\n```bash\necho hi\n```\nafter");
        // The triple backticks are gone, and a bar has taken their place: leaving them on
        // screen is what made a code block read as a log line.
        assert!(!text.iter().any(|row| row.contains("```")), "{text:?}");
        let open = &text[1];
        assert!(open.starts_with("┌─ bash"), "the language labels the block: {open:?}");
        assert_eq!(text[2].trim(), "echo hi", "{text:?}");
        assert!(text[3].starts_with('└'), "{text:?}");
    }

    #[test]
    fn a_fence_without_a_language_still_gets_a_bar() {
        // An unlabelled fence is the common case in a quick answer, and dropping the bar
        // would leave the code indistinguishable from indented prose.
        let text = rendered("```\nplain\n```");
        assert!(text[0].starts_with('┌'), "{text:?}");
        assert_eq!(text[1].trim(), "plain", "{text:?}");
        assert!(text[2].starts_with('└'), "{text:?}");
    }

    #[test]
    fn an_unterminated_fence_keeps_its_frame() {
        // This is the stream tail: the closing fence has not arrived yet. The block has to
        // look like code now, or it changes shape when the last line lands.
        let text = rendered("text\n```py\nprint(1)");
        assert!(text.iter().any(|row| row.contains("print(1)")), "{text:?}");
        assert!(text.iter().all(|row| !row.contains("```")), "{text:?}");
    }

    #[test]
    fn an_unknown_language_is_left_plain() {
        // Colourising an unknown fence would colour prose, which reads worse than no colour.
        let rows = render("```text\njust words here\n```", 80);
        let code = find(&rows, "just words");
        assert!(code.spans.iter().all(|span| span.style == Style::plain()), "{:?}", code);
    }

    #[test]
    fn shell_commands_get_their_flags_and_strings_coloured() {
        let rows = render("```sh\ngit commit -m \"fix\" --amend\n```", 80);
        let line = find(&rows, "git commit");
        assert!(line.spans.iter().any(|s| s.style.fg == Color::SyntaxString), "{:?}", line.spans);
        assert!(line.spans.iter().any(|s| s.style.fg == Color::SyntaxKeyword), "{:?}", line.spans);
    }

    #[test]
    fn comments_are_dimmer_than_the_code() {
        let rows = render("```python\nx = 1  # note\n```", 80);
        let line = find(&rows, "note");
        assert!(line.spans.iter().any(|s| s.text.contains("note") && s.style.fg == Color::Dim));
    }

    #[test]
    fn a_hash_inside_shell_word_is_not_a_comment() {
        // `${a#b}` is code; colouring from the `#` would dim the rest of the line.
        let rows = render("```sh\necho ${a#b}\n```", 80);
        let line = find(&rows, "a#b");
        assert!(
            !line.spans.iter().any(|s| s.text.contains("#b}") && s.style.fg == Color::Dim),
            "{:?}",
            line.spans
        );
    }

    #[test]
    fn a_table_gets_a_border_and_alignment() {
        let rows = render("| lang | year |\n|:-----|-----:|\n| rust | 2015 |", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].starts_with('┌'), "{text:?}");
        assert!(text[1].contains("lang"), "{text:?}");
        assert!(text[2].starts_with('├'), "{text:?}");
        assert!(text.iter().any(|row| row.contains("rust")), "{text:?}");
        assert!(text.last().unwrap().starts_with('└'), "{text:?}");
        // A right-aligned column pads on the left; the pipe count is the same either way.
        let header = &text[1];
        assert_eq!(header.matches('│').count(), 3, "{header:?}");
        assert!(text[1].contains("│ lang"), "{header:?}");
        assert!(text[1].contains("year │"), "right aligned: {header:?}");
    }

    #[test]
    fn a_streamed_half_table_is_not_a_table_yet() {
        // Without the separator the pipes are just text; eating them would make the answer
        // flicker as the separator row streams in.
        let rows = render("| lang | year |", 80);
        assert_eq!(rendered_rows(&rows), vec!["| lang | year |"]);
    }

    #[test]
    fn a_table_too_wide_for_the_terminal_falls_back_to_text() {
        let wide = "| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb |\n|---|---|\n| 1 | 2 |";
        let rows = render(wide, 30);
        let text = rendered_rows(&rows);
        assert!(!text.iter().any(|row| row.contains('│')), "{text:?}");
    }

    #[test]
    fn nested_lists_keep_their_shape_and_indent_their_wrapping() {
        let rows = render("- top\n  - nested\n    - deeper", 80);
        let text = rendered_rows(&rows);
        assert_eq!(text[0], "• top");
        assert_eq!(text[1], "  • nested");
        assert_eq!(text[2], "    • deeper");
        // A wrapped item lines up under its text, not under the bullet. The indent is on the
        // line itself (`hang`), so whichever pass wraps it produces the same shape.
        assert_eq!(rows[1].hang, 4, "the marker's width is the hang");
        let long = render("  - a nested item long enough that it has to wrap at this width", 30);
        let wrapped = crate::ui::screen::wrap_line(&long[0], 30);
        assert!(wrapped.len() > 1, "it wrapped: {:?}", long[0].text());
        assert!(
            wrapped[1].text().starts_with("    "),
            "continuation lines up under the text: {:?}",
            wrapped[1].text()
        );
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let rows = render("1. first\n2. second", 80);
        assert_eq!(rendered_rows(&rows), vec!["1. first", "2. second"]);
    }

    #[test]
    fn task_lists_get_boxes() {
        let rows = render("- [ ] todo\n- [x] done", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].contains("☐ todo"), "{text:?}");
        assert!(text[1].contains("☑ done"), "{text:?}");
    }

    #[test]
    fn quotes_nest_by_depth() {
        let rows = render("> outer\n>> inner", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].starts_with("│ outer"), "{text:?}");
        assert!(text[1].starts_with("│ │ inner"), "{text:?}");
    }

    #[test]
    fn headings_are_emphasised() {
        let rows = render("# Title\n## Sub", 80);
        assert_eq!(rendered_rows(&rows), vec!["Title", "Sub"]);
        assert!(rows[0].spans[0].style.bold);
        assert_eq!(rows[0].spans[0].style.fg, Color::Cyan);
    }

    #[test]
    fn rules_lose_their_syntax() {
        for input in ["---", "***", "___", "- - -"] {
            let rows = render(input, 80);
            assert_eq!(rendered_rows(&rows), vec!["─".repeat(24)], "{input}");
        }
        // But a rule inside a fenced block is code.
        let rows = render("```\n---\n```", 80);
        assert_eq!(rendered_rows(&rows)[1], "  ---");
    }

    #[test]
    fn blank_line_runs_collapse() {
        assert_eq!(rendered("a\n\n\n\nb"), vec!["a", "", "b"]);
        // And none are left dangling at the end.
        assert_eq!(rendered("a\n\n\n"), vec!["a"]);
    }

    #[test]
    fn underscores_inside_a_name_are_not_italic() {
        assert_eq!(rendered("foo_bar_baz"), vec!["foo_bar_baz"]);
    }

    #[test]
    fn strikethrough_keeps_its_text() {
        let rows = render("~~gone~~ kept", 80);
        assert_eq!(rows[0].text(), "gone kept");
        assert_eq!(span(&rows, "gone").style.fg, Color::Dim);
    }

    #[test]
    fn a_bare_url_is_underlined_without_its_trailing_stop() {
        let rows = render("see https://example.com/a_b.", 80);
        let line = &rows[0];
        let url = line
            .spans
            .iter()
            .find(|span| span.text.starts_with("https://"))
            .expect("the url is its own span");
        assert!(url.style.underline);
        assert_eq!(url.text, "https://example.com/a_b");
        // The sentence's full stop is left in place.
        assert_eq!(rows[0].text(), "see https://example.com/a_b.");
    }

    #[test]
    fn an_image_shows_its_alt_text() {
        let rows = render("![a chart](https://x/y.png)", 80);
        let text = rows[0].text();
        assert!(text.contains("a chart"), "{text}");
        assert!(!text.contains("https://x/y.png"), "the url is not shown: {text}");
    }

    #[test]
    fn nothing_interprets_ansii_from_the_model() {
        let rows = render("\u{1b}[31mred\u{1b}[0m", 80);
        assert!(rows.iter().all(|row| !row.text().contains('\u{1b}')));
    }

    /// Render every prefix of `text` the way a stream would, and hand back each frame.
    fn frames(text: &str) -> Vec<Vec<String>> {
        (1..=text.len())
            .filter(|end| text.is_char_boundary(*end))
            .map(|end| {
                let lines = crate::ui::screen::wrap_all(&render(&text[..end], 60), 60);
                lines.iter().map(Line::text).collect()
            })
            .collect()
    }

    #[test]
    fn a_table_that_has_appeared_never_changes_shape() {
        // The separator is what turns these rows into a table, and it arrives one character
        // at a time. The grid must not appear, vanish and reappear as the dashes land: that
        // is a whole block flashing under the reader on every keystroke of the model's
        // output. So the assertion is monotonicity — once a frame has a grid, every later
        // frame has the same one, growing only at the bottom.
        let mut seen: Option<Vec<String>> = None;
        for frame in frames("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n") {
            let grid: Vec<String> = frame.iter().filter(|row| row.starts_with('│')).cloned().collect();
            if grid.is_empty() {
                // Not a table yet: the pipes are still text, which is readable either way.
                assert!(!frame.iter().any(|row| row.starts_with('┌')), "border without cells: {frame:?}");
                continue;
            }
            match &seen {
                None => {
                    // The first grid must be the widest one — the header row, with its two
                    // columns — or the columns would re-space as more rows arrive.
                    assert_eq!(grid[0].matches('│').count(), 3, "{frame:?}");
                }
                Some(previous) => {
                    assert!(
                        grid.len() >= previous.len(),
                        "the table shrank: {previous:?} -> {grid:?}"
                    );
                    // Rows already drawn keep their column layout. A cell's *text* fills in
                    // as its characters arrive — that is the point of streaming — but the
                    // pipes must not move, because that is the grid re-spacing itself.
                    for (was, now) in previous.iter().zip(&grid) {
                        let bars = |row: &String| row.match_indices('│').map(|(at, _)| at).collect::<Vec<_>>();
                        assert_eq!(bars(was), bars(now), "columns moved: {was:?} -> {now:?}");
                    }
                }
            }
            seen = Some(grid);
        }
        assert!(seen.is_some(), "the table never appeared");
    }

    #[test]
    fn a_streamed_fence_never_shows_its_backticks() {
        // The opening fence turns into a bar immediately. Leaving the backticks visible and
        // swapping them for a bar at the end would be the block changing shape under the
        // reader's eyes.
        for frame in frames("```rust\nfn f() {}\n```") {
            assert!(
                !frame.iter().any(|row| row.contains("```")),
                "a fence marker reached the screen: {frame:?}"
            );
        }
    }

    #[test]
    fn a_bullet_is_never_left_alone_on_its_row() {
        // A plain space after the marker is a break opportunity, so at a narrow width the
        // bullet ends up on a row of its own with the text under it — which reads as a lost
        // item rather than a list. The marker is glued to the first word instead.
        let source = "- a list item long enough to need wrapping at a narrow width";
        for width in [28usize, 20, 16] {
            let lines = crate::ui::screen::wrap_all(&render(source, width), width);
            assert!(
                lines[0].text().trim_end().len() > 1,
                "width {width}: the bullet is alone: {:?}",
                lines.iter().map(Line::text).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn wrapping_the_same_line_twice_gives_the_same_shape() {
        // The screen wraps whatever it is given, and it may be given a line that markdown
        // already wrapped (a resize, or a block re-rendered at a new width). Wrapping has to
        // be idempotent, or a list item loses its indent on the second pass — the "at forty"
        // row that started at column zero instead of lining up under the text.
        let source = "- a list item that is definitely long enough to wrap at this width\n\n> and a quote long enough to wrap too";
        for width in [24usize, 40] {
            let once = crate::ui::screen::wrap_all(&render(source, width), width);
            let twice: Vec<String> =
                crate::ui::screen::wrap_all(&once, width).iter().map(Line::text).collect();
            let once_text: Vec<String> = once.iter().map(Line::text).collect();
            assert_eq!(once_text, twice, "wrapping twice changed the shape at width {width}");
        }
    }

    #[test]
    fn nothing_wraps_past_its_width_once_the_screen_has_wrapped_it() {
        let source = "- a list item long enough to wrap\n\n> a quote that also wraps around\n\n| a | b |\n|---|---|\n| one | two |";
        for width in [20usize, 40, 80] {
            for line in crate::ui::screen::wrap_all(&render(source, width), width) {
                assert!(util::width(&line.text()) <= width, "{width}: {:?}", line.text());
            }
        }
    }
}
