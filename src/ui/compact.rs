//! Turning tool results into compact transcript blocks.
//!
//! Three shapes, all collapsible with Ctrl+O and all re-collapsed when a new task starts
//! (see [`crate::ui::screen::Screen::collapse_all`]):
//!
//! * command output — the last 5 lines, with the `✓ $ command` header and any footer note
//!   (exit code, temp-file path) always visible,
//! * a file diff — red and green with line numbers, previewed at 14 rows,
//! * a plain file note — one header line plus the tail of the result.
//!
//! Text is sanitized before it becomes a block, so nothing a command printed can move the
//! cursor or recolour the UI, and styling is expressed as spans rather than escape codes.

use crate::config::Defaults;
use crate::tools::{Display, ToolOutput};
use crate::ui::diff;
use crate::ui::screen::{Bg, Block, Line, Span, Style};
use crate::ui::theme::{Color, Theme};
use crate::util;

/// Build the transcript block for a finished tool call.
///
/// The header rows are pinned: collapsing hides the *output*, never the fact of what ran
/// or which file changed.
pub fn tool_block(name: &str, arguments: &serde_json::Value, output: &ToolOutput) -> Block {
    let theme = Theme::default();
    let mut lines: Vec<Line> = Vec::new();
    let mut head = 1usize;
    let mut tail = 0usize;
    let (mark_style, mark) = if output.is_error {
        (Style::bold(Color::Red), "×")
    } else {
        (Style::bold(Color::Green), "✓")
    };
    let mark_span = Span::new(mark, mark_style);

    match &output.display {
        Display::Command { footer, .. } => {
            let command = arguments.get("command").and_then(|v| v.as_str()).unwrap_or(name);
            lines.extend(command_header(&mark_span, command));
            lines.push(Line::blank());
            lines.extend(
                plain_text_lines(&output.content, Style::plain()),
            );
            for note in footer {
                lines.push(Line::new(note.clone(), Style::new(Color::Dim)));
                tail += 1;
            }
        }
        Display::Diff { .. } => {
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(" "),
                Span::plain("修改"),
                Span::plain(" "),
                Span::new(theme_path(arguments), Style::new(Color::Cyan)),
            ]));
            // The `+3 −1` summary row belongs to the head: the counts are the point.
            let mut rows = diff::render(&theme, &output.display, 120);
            if !rows.is_empty() {
                lines.push(rows.remove(0));
                head = 2;
            }
            lines.extend(rows);
        }
        Display::File { verb, path } => {
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(" "),
                Span::plain(*verb),
                Span::plain(" "),
                Span::new(util::one_line(path), Style::new(Color::Cyan)),
            ]));
            lines.extend(plain_text_lines(&output.content, Style::new(Color::Output)));
        }
        Display::None => {
            lines.push(Line::spans(vec![mark_span.clone(), Span::plain(format!(" {name}"))]));
            lines.extend(plain_text_lines(&output.content, Style::plain()));
        }
    }
    Block::collapsible(lines, head, tail, Defaults::COMMAND_PREVIEW_LINES)
}

/// Render tool text as plain lines, stripping anything that could move the cursor or
/// recolour the UI. Tool output is untrusted: a file may contain escape sequences, and a
/// command may print them on purpose.
fn plain_text_lines(text: &str, style: Style) -> Vec<Line> {
    util::sanitize(text)
        .lines()
        .map(|line| Line::new(line.to_string(), style))
        .collect()
}

/// The `✓ $ command` header as styled runs. A collapsed multi-line command shows its first
/// line plus a note, because printing the whole body would defeat collapsing.
pub fn command_header(mark: &Span, command: &str) -> Vec<Line> {
    let _ = Theme::default();
    let mut spans = vec![
        Span::new(mark.text.clone(), Style { bold: true, ..mark.style }),
        Span::plain(" "),
        Span::new("$ ", Style::new(Color::Magenta)),
    ];
    let lines: Vec<&str> = command.split('\n').collect();
    spans.extend(highlight_spans(lines[0]));
    if lines.len() > 1 {
        spans.push(Span::new(
            format!(" …（{} 行命令）", lines.len()),
            Style::new(Color::Dim),
        ));
    }
    vec![Line::spans(spans)]
}

/// The user's own message, echoed with a marker.
pub fn user_lines(text: &str) -> Vec<Line> {
    let body = util::sanitize(text);
    let mut rows = body.lines();
    let first = rows.next().unwrap_or("");
    let mut lines = vec![Line::spans(vec![
        Span::new("› ", Style::bold(Color::Cyan)),
        Span::plain(first.to_string()),
    ])];
    lines.extend(rows.map(|line| Line::plain(line.to_string())));
    lines.push(Line::blank());
    lines
}

/// A short system note (compaction notices, errors, command feedback).
pub fn note_lines(text: &str, style: Style) -> Vec<Line> {
    let mut out: Vec<Line> = util::sanitize(text)
        .lines()
        .map(|line| Line::new(line.to_string(), style))
        .collect();
    out.push(Line::blank());
    out
}

/// The one-line marker that replaces the live thinking preview once a turn ends.
pub fn thinking_done_lines() -> Vec<Line> {
    vec![Line::new("思考完成", Style::new(Color::Dim)), Line::blank()]
}

/// How many rows the collapsed command preview keeps.
pub fn command_preview() -> usize {
    Defaults::COMMAND_PREVIEW_LINES
}

/// Light shell highlighting as styled runs.
///
/// Display-only: joining the spans must give back the original characters exactly, so the
/// function never drops or rewrites a byte of the command.
pub fn highlight_spans(line: &str) -> Vec<Span> {
    const KEYWORDS: [&str; 47] = [
        "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac",
        "in", "function", "return", "echo", "cd", "export", "source", "set", "unset",
        "local", "readonly", "shift", "trap", "exit", "test", "git", "cargo", "rg", "fd",
        "grep", "find", "sed", "awk", "cat", "ls", "rm", "mv", "cp", "mkdir", "touch",
        "npm", "bun", "node", "python3", "curl", "wget",
    ];
    let mut spans: Vec<Span> = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    fn flush(spans: &mut Vec<Span>, word: &mut String) {
        if word.is_empty() {
            return;
        }
        let style = if KEYWORDS.contains(&word.as_str()) {
            Style::new(Color::Output)
        } else {
            Style::plain()
        };
        spans.push(Span::new(std::mem::take(word), style));
    }
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(active) => {
                word.push(c);
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                    continue;
                }
                if c == active {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    flush(&mut spans, &mut word);
                    let mut literal = String::from(c);
                    // Keep the quoted body with its quote so the run stays one span.
                    while let Some(next) = chars.next() {
                        literal.push(next);
                        if next == '\\' {
                            if let Some(escaped) = chars.next() {
                                literal.push(escaped);
                            }
                            continue;
                        }
                        if next == c {
                            break;
                        }
                    }
                    spans.push(Span::new(literal, Style::new(Color::DiffAddedText)));
                }
                '#' => {
                    flush(&mut spans, &mut word);
                    let rest: String = chars.by_ref().collect();
                    spans.push(Span::new(format!("#{rest}"), Style::new(Color::Dim)));
                    break;
                }
                c if c.is_whitespace() || "|&;()<>=".contains(c) => {
                    flush(&mut spans, &mut word);
                    spans.push(Span::new(c.to_string(), Style::new(Color::Magenta)));
                }
                other => word.push(other),
            },
        }
    }
    flush(&mut spans, &mut word);
    if spans.is_empty() {
        spans.push(Span::plain(String::new()));
    }
    spans
}

/// The diff background style for a row kind.
pub fn diff_style(kind: diff::Kind) -> Style {
    match kind {
        diff::Kind::Added => Style::with_bg(Color::DiffAddedText, Bg::Added),
        diff::Kind::Removed => Style::with_bg(Color::DiffRemovedText, Bg::Removed),
        diff::Kind::Context => Style::new(Color::Dim),
    }
}

fn theme_path(arguments: &serde_json::Value) -> String {
    arguments
        .get("path")
        .and_then(|value| value.as_str())
        .map(util::one_line)
        .unwrap_or_else(|| "…".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::plain;

    #[test]
    fn a_command_result_collapses_to_its_tail() {
        let output = ToolOutput {
            content: (0..20).map(|i| format!("row {i}\n")).collect(),
            display: Display::Command { expanded: false, footer: vec!["耗时 0.4s".into()] },
            is_error: false,
        };
        let block = tool_block("bash", &serde_json::json!({"command": "ls -l"}), &output);
        assert!(block.is_collapsible());
        let text = plain(&block.render(80));
        assert!(text[0].starts_with("✓ "), "{text:?}");
        assert!(text[0].contains("$ ls -l"), "{text:?}");
        assert!(text.iter().any(|line| line.contains("已收起")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("row 19")), "{text:?}");
        // The footer note is the last row, visible even when collapsed.
        assert!(text.last().unwrap().contains("耗时 0.4s"), "{text:?}");
    }

    #[test]
    fn a_failed_command_is_marked_and_reports_the_exit_code() {
        let output = ToolOutput {
            content: "boom\n[退出码 3]\n".into(),
            display: Display::Command { expanded: false, footer: vec!["退出码 3".into()] },
            is_error: true,
        };
        let text = plain(&tool_block("bash", &serde_json::json!({"command": "false"}), &output).render(80));
        assert!(text[0].starts_with("× "), "{text:?}");
        assert!(text.iter().any(|line| line.contains("退出码 3")), "{text:?}");
    }

    #[test]
    fn a_multi_line_command_shows_its_first_line_and_a_note() {
        let mark = Span::new("✓", Style::bold(Color::Green));
        let header = command_header(&mark, "echo one\necho two\necho three");
        assert_eq!(header.len(), 1);
        let text = plain(&header);
        assert!(text[0].contains("echo one"), "{text:?}");
        assert!(!text[0].contains("echo two"));
        assert!(text[0].contains("…（3 行命令）"), "{text:?}");
    }

    #[test]
    fn highlighting_is_display_only() {
        for command in [
            "git log --oneline -5 # show history",
            "echo 'a b' \"c d\"",
            "cat a.txt | head -3",
            "x=1 y='two'",
            "no-keywords-here --flag=value",
            "trailing-text",
        ] {
            let joined: String = highlight_spans(command).iter().map(|s| s.text.as_str()).collect();
            assert_eq!(joined, command, "highlighting changed {command:?}");
        }
    }

    #[test]
    fn keywords_and_comments_get_distinct_styles() {
        let spans = highlight_spans("git log # why");
        assert_eq!(spans[0].text, "git");
        assert_eq!(spans[0].style.fg, Color::Output);
        assert!(spans.iter().any(|s| s.text == "# why" && s.style.fg == Color::Dim));
    }

    #[test]
    fn a_diff_block_keeps_red_and_green_rows() {
        let output = ToolOutput {
            content: "已修改 a.rs".into(),
            display: crate::ui::diff::for_edit("one\ntwo\n", "one\nTWO\n"),
            is_error: false,
        };
        let block = tool_block("edit", &serde_json::json!({"path": "a.rs"}), &output);
        // The header and the +/- summary stay visible when collapsed.
        let collapsed = plain(&block.render(80));
        assert!(collapsed[0].contains("修改 a.rs"), "{collapsed:?}");
        assert!(collapsed[1].contains("+1") && collapsed[1].contains("−1"), "{collapsed:?}");
        // A diff this small fits the preview, so nothing is hidden and there is no note.
        assert!(!collapsed.iter().any(|line| line.contains("已收起")), "{collapsed:?}");
        // Rows carry the codex tints. The row text is ` - 2 │ two`, so match on the payload.
        let painted = block.render(80);
        let removed = painted
            .iter()
            .find(|line| line.text().contains("two"))
            .unwrap_or_else(|| panic!("the removed row is missing: {painted:?}"));
        assert_eq!(removed.spans[0].style.bg, Bg::Removed);
        let added = painted
            .iter()
            .find(|line| line.text().contains("TWO"))
            .unwrap_or_else(|| panic!("the added row is missing: {painted:?}"));
        assert_eq!(added.spans[0].style.bg, Bg::Added);
        // The removed and added rows must not be the same line.
        assert!(!std::ptr::eq(removed, added));
    }

    #[test]
    fn a_diff_taller_than_the_preview_gets_a_collapse_note() {
        let before: String = (0..40).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..40).map(|i| format!("new {i}\n")).collect();
        let output = ToolOutput {
            content: "已修改 big.rs".into(),
            display: crate::ui::diff::for_edit(&before, &after),
            is_error: false,
        };
        let text = plain(&tool_block("edit", &serde_json::json!({"path": "big.rs"}), &output).render(80));
        assert!(text.iter().any(|line| line.contains("已收起")), "{text:?}");
        // The header and the counts are still the first two rows.
        assert!(text[0].contains("修改 big.rs"), "{text:?}");
        assert!(text[1].contains("+40") && text[1].contains("−40"), "{text:?}");
    }

    #[test]
    fn a_file_result_shows_a_header_and_a_short_preview() {
        let output = ToolOutput {
            content: (0..20).map(|i| format!("entry {i}\n")).collect(),
            display: Display::File { verb: "列出", path: "src".into() },
            is_error: false,
        };
        let text = plain(&tool_block("ls", &serde_json::json!({}), &output).render(120));
        assert!(text.iter().any(|line| line.contains("列出 src")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("已收起")), "{text:?}");
        assert!(!text[0].contains("已收起"));
    }

    #[test]
    fn user_input_is_echoed_with_a_marker() {
        let lines = user_lines("hello\nworld");
        let text = plain(&lines);
        assert!(text[0].starts_with("› hello"), "{text:?}");
        assert_eq!(text[1], "world");
        assert_eq!(text.last().unwrap(), "");
    }

    #[test]
    fn control_characters_in_tool_output_are_neutralised() {
        let output = ToolOutput {
            content: "safe\u{7}bell\u{1b}[2J".into(),
            display: Display::None,
            is_error: false,
        };
        let text = plain(&tool_block("read", &serde_json::json!({}), &output).render(80));
        assert!(text.iter().all(|line| !line.contains('\u{1b}') && !line.contains('\u{7}')));
    }

    #[test]
    fn every_line_contains_no_escape_sequences() {
        let output = ToolOutput {
            content: "ok\n".into(),
            display: Display::Command { expanded: false, footer: vec!["退出码 1".into()] },
            is_error: true,
        };
        for line in tool_block("bash", &serde_json::json!({"command": "false"}), &output).render(60) {
            assert!(!line.text().contains('\u{1b}'), "{line:?}");
        }
    }
}
