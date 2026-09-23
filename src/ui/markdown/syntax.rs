//! Tree-sitter highlighting for the five languages a fence is worth a real grammar for.
//!
//! Rust, JavaScript, TypeScript, Bash, Kotlin, Python and JSON. The queries and the parsers
//! come from the grammar crates; the colours are still this program's, so a keyword is the
//! same mauve in a Rust fence as in a Python one. Any other language is `None` and the
//! hand-written scanner in [`super::code`] keeps it.

use std::sync::OnceLock;

use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

use crate::ui::text::{Span, Style};
use crate::ui::theme::Color;

const MAX_BYTES: usize = 64 * 1024;

const NAMES: &[&str] = &[
    "attribute",
    "comment",
    "comment.documentation",
    "constant",
    "constant.builtin",
    "constructor",
    "escape",
    "function",
    "function.builtin",
    "function.macro",
    "function.method",
    "keyword",
    "label",
    "number",
    "string",
    "string.special",
    "string.special.key",
    "type",
    "type.builtin",
    "variable.builtin",
];

struct Grammars {
    rust: HighlightConfiguration,
    javascript: HighlightConfiguration,
    typescript: HighlightConfiguration,
    tsx: HighlightConfiguration,
    bash: HighlightConfiguration,
    kotlin: HighlightConfiguration,
    python: HighlightConfiguration,
    json: HighlightConfiguration,
}

fn grammars() -> &'static Grammars {
    static GRAMMARS: OnceLock<Grammars> = OnceLock::new();
    GRAMMARS.get_or_init(build)
}

fn build() -> Grammars {
    Grammars {
        rust: config(
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            "",
        ),
        javascript: config(
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY,
        ),
        typescript: config(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        tsx: config(
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY,
        ),
        bash: config(tree_sitter_bash::LANGUAGE.into(), "bash", tree_sitter_bash::HIGHLIGHT_QUERY, "", ""),
        kotlin: config(
            tree_sitter_kotlin_ng::LANGUAGE.into(),
            "kotlin",
            include_str!("kotlin.scm"),
            "",
            "",
        ),
        python: config(tree_sitter_python::LANGUAGE.into(), "python", tree_sitter_python::HIGHLIGHTS_QUERY, "", ""),
        json: config(tree_sitter_json::LANGUAGE.into(), "json", include_str!("json.scm"), "", ""),
    }
}

fn config(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
    locals: &str,
) -> HighlightConfiguration {
    let mut config = HighlightConfiguration::new(language, name, highlights, injections, locals)
        .unwrap_or_else(|err| panic!("highlight query for {name} failed: {err}"));
    config.configure(NAMES);
    config
}

fn color_of(name: &str) -> Color {
    match name {
        "keyword" | "variable.builtin" => Color::SyntaxKeyword,
        "function" | "function.builtin" | "function.macro" | "function.method" => Color::SyntaxFunction,
        "type" | "type.builtin" | "constructor" | "attribute" | "label" => Color::SyntaxType,
        "string" | "string.special" | "escape" => Color::SyntaxString,
        // A JSON key is a string, but it is the name of the field. Colouring it as a call
        // keeps it apart from the value, which stays green.
        "string.special.key" => Color::SyntaxFunction,
        "comment" | "comment.documentation" => Color::SyntaxComment,
        "constant" | "constant.builtin" | "number" => Color::SyntaxNumber,
        _ => Color::Text,
    }
}

/// Highlight `lines` as one buffer, or `None` when `lang` is not one of the grammars.
///
/// `None` on a language we do claim means the parse failed; the caller falls back to the
/// hand-written scanner rather than showing a blank fence. A block past [`MAX_BYTES`] is
/// also refused: tree-sitter on a pasted log is not worth stalling the redraw.
pub fn paint(lang: Option<&str>, lines: &[String]) -> Option<Vec<Vec<Span>>> {
    let config = grammar(lang?)?;
    let source = lines.join("\n");
    if source.len() > MAX_BYTES {
        return None;
    }
    let colors: Vec<Color> = NAMES.iter().copied().map(color_of).collect();
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, source.as_bytes(), None, |_| None)
        .ok()?;
    Some(split_lines(&source, events, &colors))
}

fn grammar(lang: &str) -> Option<&'static HighlightConfiguration> {
    let set = grammars();
    Some(match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => &set.rust,
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => &set.javascript,
        "typescript" | "ts" | "mts" | "cts" => &set.typescript,
        "tsx" => &set.tsx,
        "bash" | "sh" | "shell" | "zsh" | "console" => &set.bash,
        "kotlin" | "kt" | "kts" => &set.kotlin,
        "python" | "py" => &set.python,
        "json" | "jsonc" => &set.json,
        _ => return None,
    })
}

fn split_lines(
    source: &str,
    events: impl Iterator<Item = Result<HighlightEvent, tree_sitter_highlight::Error>>,
    colors: &[Color],
) -> Vec<Vec<Span>> {
    let mut lines = vec![Vec::new()];
    let mut stack: Vec<usize> = Vec::new();
    for event in events.flatten() {
        match event {
            HighlightEvent::HighlightStart(Highlight(index)) => stack.push(index),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                // A JSON key is also matched as a string. When both captures cover the same
                // bytes, the key is the one to keep; a value stays a string.
                let color = if stack.iter().any(|&index| NAMES.get(index) == Some(&"string.special.key")) {
                    Color::SyntaxFunction
                } else {
                    stack.last().and_then(|&index| colors.get(index).copied()).unwrap_or(Color::Text)
                };
                let Ok(chunk) = std::str::from_utf8(&source.as_bytes()[start..end]) else {
                    continue;
                };
                for (index, piece) in chunk.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(Vec::new());
                    }
                    push(&mut lines, piece, color);
                }
            }
        }
    }
    lines
}

fn push(lines: &mut Vec<Vec<Span>>, text: &str, color: Color) {
    if text.is_empty() {
        return;
    }
    let line = lines.last_mut().unwrap();
    let style = Style::new(color);
    if let Some(last) = line.last_mut()
        && last.style == style
    {
        last.text.push_str(text);
        return;
    }
    line.push(Span::new(text, style));
}
