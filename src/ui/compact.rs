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
    tool_block_with(name, arguments, output, false)
}

/// The mark for a call: still running, failed, or finished.
///
/// A running call gets `●` rather than a green tick, because the tick is a claim about the
/// outcome and there is no outcome yet. pi does the same, and the difference matters: a
/// tick next to a command that is still going says it succeeded before it has.
pub fn status_mark(running: bool, failed: bool) -> (Style, &'static str) {
    if running {
        (Style::new(Color::Dim), "●")
    } else if failed {
        (Style::bold(Color::Red), "×")
    } else {
        (Style::bold(Color::Green), "✓")
    }
}

/// Like [`tool_block`], but for a call that has not finished yet: no output, no duration,
/// and a `●` mark instead of a verdict.
pub fn running_block(name: &str, arguments: &serde_json::Value) -> Block {
    let empty = ToolOutput {
        content: String::new(),
        display: arguments_display(name, arguments),
        is_error: false,
        duration: None,
    };
    tool_block_with(name, arguments, &empty, true)
}

/// The display a call would have, derived from its arguments alone. Used while running,
/// when there is no result yet to describe it.
fn arguments_display(name: &str, arguments: &serde_json::Value) -> Display {
    match name {
        "bash" => Display::Command { expanded: false, footer: Vec::new() },
        "write" | "edit" | "read" | "grep" | "find" | "ls" => Display::File {
            verb: crate::tools::verb_for(name),
            path: arguments
                .get("path")
                .or_else(|| arguments.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string(),
        },
        _ => Display::None,
    }
}

fn tool_block_with(
    name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
    running: bool,
) -> Block {
    let theme = Theme::default();
    let mut lines: Vec<Line> = Vec::new();
    let mut head = 1usize;
    let mut tail = 0usize;
    let (mark_style, mark) = status_mark(running, output.is_error);
    let mark_span = Span::new(mark, mark_style);

    match &output.display {
        Display::Command { footer, .. } => {
            let command = arguments.get("command").and_then(|v| v.as_str()).unwrap_or(name);
            lines.extend(command_header(&mark_span, command));
            lines.push(Line::blank());
            lines.extend(
                plain_text_lines(&output.content, Style::plain()),
            );
            // The duration leads the footer: it is the one thing the user compares between
            // runs, and an exit code only appears when something went wrong.
            if let Some(duration) = output.duration {
                lines.push(Line::new(
                    format!("耗时 {}", format_duration(duration)),
                    Style::new(Color::Dim),
                ));
                tail += 1;
            }
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

/// A duration in the shortest form that still says how long it was.
///
/// Sub-second is the common case, and one decimal is the resolution at which a user can
/// tell a fast command from a slow one. Minutes keep the seconds for the same reason.
pub fn format_duration(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let minutes = (seconds / 60.0).floor();
        format!("{}m{:.0}s", minutes as u64, seconds - minutes * 60.0)
    }
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

/// Replay a stored conversation into transcript blocks.
///
/// Resuming used to echo only the user's own lines, which made a resumed session look like
/// a list of questions with no answers: the assistant text, the commands and their output
/// were all in the file but never drawn. What was on screen when the session ended is what
/// has to come back.
///
/// Tool results are re-attached to the call they answer, because that is how they were
/// shown: the pairing lives in the `tool_call_id`, and a result printed on its own would
/// read as unexplained output. Stored messages carry no `duration`, so a resumed command
/// shows its output without a time — inventing one would be worse than omitting it.
pub fn replay_blocks(messages: &[crate::llm::Message]) -> Vec<crate::ui::screen::Block> {
    use crate::llm::{Block as MsgBlock, Message};

    // Tool results, keyed by the call they belong to.
    let mut results: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for message in messages {
        if let Message::Tool { tool_call_id, content, .. } = message {
            results.insert(tool_call_id.as_str(), content.as_str());
        }
    }

    let mut out: Vec<crate::ui::screen::Block> = Vec::new();
    for message in messages {
        // The environment block is bookkeeping that happens to be a user message. Filtered
        // here rather than by the caller so no replay path can forget and print it as
        // something the user said.
        if crate::agent::r#loop::is_environment_block(message) {
            continue;
        }
        match message {
            Message::User { content } => {
                let text = message.text();
                if text.trim().is_empty() && content.iter().all(|b| !matches!(b, MsgBlock::Text { .. })) {
                    continue;
                }
                if !text.trim().is_empty() {
                    push_lines(&mut out, user_lines(&text));
                }
                for block in content {
                    if let MsgBlock::Image { .. } = block {
                        push_lines(&mut out, note_lines("［图片］", Style::new(Color::Magenta)));
                    }
                }
            }
            Message::Assistant { content, .. } => {
                let mut lines: Vec<Line> = Vec::new();
                let had_thinking = content.iter().any(|b| matches!(b, MsgBlock::Thinking { .. }));
                for block in content {
                    match block {
                        MsgBlock::Text { text } => {
                            lines.extend(
                                util::sanitize(text).lines().map(|l| Line::plain(l.to_string())),
                            );
                            lines.push(Line::blank());
                        }
                        MsgBlock::ToolCall { id, name, arguments } => {
                            if !lines.is_empty() {
                                push_lines(&mut out, std::mem::take(&mut lines));
                            }
                            let content = results.get(id.as_str()).copied().unwrap_or("");
                            // A tool call keeps its own block, so the output is collapsed
                            // on resume exactly as it was when it ran.
                            out.push(tool_block(name, arguments, &stored_output(name, arguments, content)));
                        }
                        _ => {}
                    }
                }
                if had_thinking {
                    let mut prefix = thinking_done_lines();
                    prefix.extend(lines);
                    push_lines(&mut out, prefix);
                } else if !lines.is_empty() {
                    push_lines(&mut out, lines);
                }
            }
            // Already folded into the call above.
            Message::Tool { .. } | Message::System { .. } => {}
        }
    }
    out
}

/// Append `lines` to the previous block, or start a new one with a blank separator.
///
/// One message's text and thinking belong together: emitting them as separate blocks would
/// put a collapse note between two halves of the same reply.
fn push_lines(out: &mut Vec<crate::ui::screen::Block>, lines: Vec<Line>) {
    if lines.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(crate::ui::screen::Block::Lines(previous)) => previous.extend(lines),
        _ => out.push(crate::ui::screen::Block::lines(lines)),
    }
}

/// A tool result as it comes back out of the session file.
///
/// Only the text survives storage, so the display is rebuilt from the tool name and
/// arguments — the same payload [`crate::tools::ToolOutput::error_for`] builds for a
/// refused call, which is exactly the shape of "a call with a known result and no extras".
fn stored_output(name: &str, arguments: &serde_json::Value, content: &str) -> ToolOutput {
    let mut output = ToolOutput::text(content);
    output.display = match name {
        "bash" => Display::Command { expanded: false, footer: Vec::new() },
        "write" | "edit" | "read" | "grep" | "find" | "ls" => Display::File {
            verb: crate::tools::verb_for(name),
            path: arguments
                .get("path")
                .or_else(|| arguments.get("file_path"))
                .and_then(|v| v.as_str())
                .map(util::one_line)
                .unwrap_or_else(|| "…".into()),
        },
        _ => Display::None,
    };
    output
}

/// The one-line marker that replaces the live thinking preview once a turn ends.
pub fn thinking_done_lines() -> Vec<Line> {
    vec![Line::new("思考完成", Style::new(Color::Dim)), Line::blank()]
}

/// How many rows the collapsed command preview keeps.
pub fn command_preview() -> usize {
    Defaults::COMMAND_PREVIEW_LINES
}

/// The single line for a call in flight: `● $ sleep 30`.
///
/// Only the first line of a multi-line command, matching the collapsed transcript row, so
/// the running line and the finished one line up as the same thing.
pub fn running_line(name: &str, arguments: &serde_json::Value) -> Vec<Span> {
    let display = arguments_display(name, arguments);
    let (style, mark) = status_mark(true, false);
    let header = Span::new(mark, style);
    match &display {
        Display::Command { .. } => {
            let command = arguments.get("command").and_then(|v| v.as_str()).unwrap_or(name);
            // A multi-line command collapses to its first line here too, so the running row
            // and the finished header are the same shape.
            let header_line = command_header(&header, command);
            header_line.into_iter().next().map(|line| line.spans).unwrap_or_default()
        }
        Display::File { verb, path } => {
            let _ = Theme::default();
            vec![
                header,
                Span::plain(" "),
                Span::new(*verb, Style::plain()),
                Span::plain(" "),
                Span::new(crate::util::one_line(path), Style::new(Color::Cyan)),
            ]
        }
        Display::None => vec![header, Span::plain(format!(" {name}"))],
        Display::Diff { .. } => vec![header, Span::plain(format!(" {name}"))],
    }
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
    fn a_resumed_conversation_replays_the_answers_too() {
        // The bug this pins: resume replayed only the user's lines, so a restored session
        // looked like a list of questions with the answers missing — even though the file
        // held the assistant text, the command and its output all along.
        use crate::llm::{Block as MsgBlock, Message, StopReason};
        let messages = vec![
            Message::user_text("<environment>\n工作目录: /tmp\n</environment>"),
            Message::user_text("跑一下 echo"),
            Message::Assistant {
                content: vec![
                    MsgBlock::Thinking { thinking: "先跑命令".into(), signature: None },
                    MsgBlock::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                    },
                ],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                name: "bash".into(),
                content: "hi\n".into(),
            },
            Message::Assistant {
                content: vec![MsgBlock::Text { text: "输出是 hi".into() }],
                stop_reason: Some(StopReason::Stop),
            },
        ];
        let blocks: Vec<Vec<String>> =
            replay_blocks(&messages).iter().map(|block| plain(&block.render(80))).collect();
        let text = blocks.join(&"".to_string()).join("\n");

        assert!(text.contains("› 跑一下 echo"), "{text}");
        assert!(text.contains("✓ $ echo hi"), "the command must come back: {text}");
        assert!(text.contains("hi"), "the command's output must come back: {text}");
        assert!(text.contains("输出是 hi"), "the answer must come back: {text}");
        assert!(text.contains("思考完成"), "thinking stays collapsed: {text}");
        // The environment block is bookkeeping, not something the user said.
        assert!(!text.contains("<environment>"), "{text}");
    }

    #[test]
    fn a_resumed_command_claims_no_duration() {
        // The file stores the result, not how long it took. Printing a time would be
        // inventing one, and `0.0s` reads as a measurement rather than as "unknown".
        use crate::llm::{Block as MsgBlock, Message};
        let messages = vec![Message::Assistant {
            content: vec![MsgBlock::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "sleep 5"}),
            }],
            stop_reason: None,
        }];
        let text: Vec<String> = replay_blocks(&messages)
            .iter()
            .flat_map(|block| plain(&block.render(80)))
            .collect();
        assert!(!text.iter().any(|line| line.contains("耗时")), "{text:?}");
    }

    #[test]
    fn a_command_result_collapses_to_its_tail() {
        let output = ToolOutput {
            content: (0..20).map(|i| format!("row {i}\n")).collect(),
            display: Display::Command { expanded: false, footer: vec!["耗时 0.4s".into()] },
            is_error: false,
            duration: None,
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
    fn a_running_call_is_marked_as_unfinished() {
        // A tick is a claim about the outcome, and a call that has not finished has no
        // outcome. Showing one while the command runs would say it succeeded before it did.
        let (style, mark) = status_mark(true, false);
        assert_eq!(mark, "●");
        assert!(!style.bold, "a running mark is not a verdict, so it is not bold");

        // Finished calls keep the verdict marks.
        assert_eq!(status_mark(false, false).1, "✓");
        assert_eq!(status_mark(false, true).1, "×");
        assert!(status_mark(false, false).0.bold);
        assert!(status_mark(false, true).0.bold);

        // The live line names the command, and a multi-line one collapses to its first line
        // so it matches the header that replaces it.
        let line = running_line("bash", &serde_json::json!({"command": "ls -l\necho done"}));
        let text: String = line.iter().map(|span| span.text.as_str()).collect();
        assert!(text.starts_with("● $ ls -l"), "{text:?}");
        assert!(!text.contains("echo done"), "only the first line is shown: {text:?}");
    }

    #[test]
    fn a_command_result_reports_how_long_it_took() {
        // The duration is a property of the call, and the collapsed view is the one the user
        // compares between runs, so it has to survive collapsing.
        let mut output = ToolOutput {
            content: "done\n".into(),
            display: Display::Command { expanded: false, footer: Vec::new() },
            is_error: false,
            duration: None,
        };
        let block = tool_block("bash", &serde_json::json!({"command": "sleep 1"}), &output);
        let text = plain(&block.render(80));
        assert!(!text.iter().any(|line| line.contains("耗时")), "no time, no claim");

        output.duration = Some(std::time::Duration::from_millis(3400));
        let block = tool_block("bash", &serde_json::json!({"command": "sleep 1"}), &output);
        let text = plain(&block.render(80));
        assert!(text.iter().any(|line| line == "耗时 3.4s"), "{text:?}");
    }

    #[test]
    fn durations_read_at_the_scale_they_are_at() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(400)), "0.4s");
        assert_eq!(format_duration(Duration::from_millis(3400)), "3.4s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59.0s");
        // Past a minute the seconds still matter, so they are kept.
        assert_eq!(format_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(format_duration(Duration::from_secs(95)), "1m35s");
    }

    #[test]
    fn a_failed_command_is_marked_and_reports_the_exit_code() {
        let output = ToolOutput {
            content: "boom\n[退出码 3]\n".into(),
            display: Display::Command { expanded: false, footer: vec!["退出码 3".into()] },
            is_error: true,
            duration: None,
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
            duration: None,
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
            duration: None,
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
            duration: None,
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
            duration: None,
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
            duration: None,
        };
        for line in tool_block("bash", &serde_json::json!({"command": "false"}), &output).render(60) {
            assert!(!line.text().contains('\u{1b}'), "{line:?}");
        }
    }
}
