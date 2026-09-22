//! A small markdown renderer for assistant text.
//!
//! The model writes markdown; the transcript used to show the marks (`**`, fences, `#`)
//! as characters. This turns the common forms into spans the screen already knows how to
//! paint. It is not a CommonMark implementation: tables, footnotes and raw HTML stay
//! text. What it does cover is what models actually emit — headings, emphasis, inline
//! code, fenced blocks, lists, quotes and links.
//!
//! An unclosed mark is left as characters. A stream is rendered on every token, and
//! eating a `**` that has not closed yet would make the preview flicker between styled
//! and literal as the rest of the word arrives.

use crate::ui::screen::{Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

/// Render `text` as styled lines. ANSI in the source is stripped first: the styles
/// below are the only colour this text is allowed to carry.
pub fn render(text: &str) -> Vec<Line> {
    let clean = util::sanitize(text);
    let mut out = Vec::new();
    let mut fenced = false;
    for line in clean.split('\n') {
        if is_fence(line) {
            fenced = !fenced;
            out.push(Line::new(line.trim_end(), Style::new(Color::Dim)));
            continue;
        }
        if fenced {
            out.push(Line::new(line.trim_end(), Style::new(Color::Green)));
            continue;
        }
        out.push(block_line(line));
    }
    out
}

fn is_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn block_line(line: &str) -> Line {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Line::blank();
    }
    if is_rule(trimmed) {
        return Line::new("———", Style::new(Color::Dim));
    }
    if let Some(rest) = heading(trimmed) {
        return Line::spans(inline(rest, Style::bold(Color::Cyan)));
    }
    if let Some(rest) = trimmed.trim_start().strip_prefix('>') {
        let body = rest.strip_prefix(' ').unwrap_or(rest);
        let mut spans = vec![Span::new("│ ", Style::new(Color::Dim))];
        spans.extend(inline(body, Style::new(Color::Dim)));
        return Line::spans(spans);
    }
    if let Some((marker, body)) = list_item(trimmed) {
        let mut spans = vec![Span::new(marker, Style::new(Color::Dim))];
        spans.extend(inline(body, Style::plain()));
        return Line::spans(spans);
    }
    Line::spans(inline(trimmed, Style::plain()))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn heading(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
        Some(line[hashes + 1..].trim_end())
    } else {
        None
    }
}

/// `"- item"` / `"1. item"` → the marker (kept, including its indent) and the body.
fn list_item(line: &str) -> Option<(String, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let rest = &line[indent_len..];
    let bytes = rest.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        let marker = format!("{}{} ", &line[..indent_len], rest.chars().next().unwrap_or('-'));
        return Some((marker, &rest[2..]));
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits < 6 {
        let after = &rest[digits..];
        if let Some(body) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            let marker = format!("{}{} ", &line[..indent_len], &rest[..rest.len() - body.len()]);
            return Some((marker, body));
        }
    }
    None
}

fn inline(text: &str, style: Style) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == '`') {
                push(&mut spans, &mut buf, style);
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                if !code.is_empty() {
                    spans.push(Span::new(code, Style::new(Color::Green)));
                }
                i += end + 2;
                continue;
            }
        }
        if chars[i] == '['
            && let Some((label, url, next)) = link_at(&chars, i)
        {
            push(&mut spans, &mut buf, style);
            if !label.is_empty() {
                spans.push(Span::new(label.clone(), Style::new(Color::Cyan)));
            }
            if !url.is_empty() && url != label {
                spans.push(Span::plain(" "));
                spans.push(Span::new(url, Style::new(Color::Dim)));
            }
            i = next;
            continue;
        }
        if let Some((marker, end)) = closer(&chars, i) {
            push(&mut spans, &mut buf, style);
            let inner: String = chars[i + marker.len()..end].iter().collect();
            let mut inner_style = style;
            if marker == "**" || marker == "__" {
                inner_style.bold = true;
            } else {
                inner_style.fg = Color::Dim;
            }
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

/// A `**` / `__` / `*` / `_` that has a matching closer, and the index where the closer starts.
fn closer(chars: &[char], at: usize) -> Option<(&'static str, usize)> {
    let marker = if starts_with(chars, at, &['*', '*']) {
        "**"
    } else if starts_with(chars, at, &['_', '_']) {
        "__"
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
            if marker == "_" && j < chars.len() && chars.get(j + 1).is_some_and(|c| c.is_ascii_alphanumeric()) {
                j += 1;
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.text()
    }

    #[test]
    fn plain_prose_is_unchanged() {
        let lines = render("hello\nworld");
        assert_eq!(lines.iter().map(text).collect::<Vec<_>>(), vec!["hello", "world"]);
        assert_eq!(lines[0].spans[0].style, Style::plain());
    }

    #[test]
    fn emphasis_code_and_links_lose_their_marks() {
        let line = &render("see **bold** and `code` plus [docs](https://example.com)")[0];
        assert_eq!(text(line), "see bold and code plus docs https://example.com");
        assert!(line.spans.iter().any(|span| span.text == "bold" && span.style.bold));
        assert!(line.spans.iter().any(|span| span.text == "code" && span.style.fg == Color::Green));
        assert!(line.spans.iter().any(|span| span.text == "docs" && span.style.fg == Color::Cyan));
    }

    #[test]
    fn an_unclosed_mark_stays_literal() {
        assert_eq!(text(&render("still **open")[0]), "still **open");
    }

    #[test]
    fn a_fence_is_code_and_does_not_interpret_marks() {
        let lines = render("```rs\nlet x = **no**;\n```");
        assert_eq!(text(&lines[0]), "```rs");
        assert_eq!(lines[1].spans[0].style.fg, Color::Green);
        assert_eq!(text(&lines[1]), "let x = **no**;");
        assert_eq!(text(&lines[2]), "```");
    }

    #[test]
    fn headings_lists_and_quotes_keep_their_text() {
        let lines = render("# Title\n- one\n> said");
        assert_eq!(text(&lines[0]), "Title");
        assert!(lines[0].spans[0].style.bold);
        assert_eq!(text(&lines[1]), "- one");
        assert_eq!(text(&lines[2]), "│ said");
    }

    #[test]
    fn underscores_inside_a_name_are_not_italic() {
        assert_eq!(text(&render("foo_bar_baz")[0]), "foo_bar_baz");
    }
}
