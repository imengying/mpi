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
use crate::ui::screen::{Block, Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

/// The mark for a call that is still running: not a verdict, just "not finished".
///
/// A running call gets `●` rather than a green tick, because the tick is a claim about the
/// outcome and there is no outcome yet. pi does the same, and the difference matters: a
/// tick next to a command that is still going says it succeeded before it has.
pub fn running_mark() -> (Style, &'static str) {
    (Style::new(Color::Dim), "●")
}

/// The mark for a call that has settled.
///
/// `⊘` is for the two statuses that produced no result at all — the user stopped the call,
/// or it was never started. Those are not failures, and marking them `×` said they were:
/// a command the user chose to stop did not go wrong, and one that never ran cannot have.
pub fn status_mark(status: crate::llm::ToolStatus) -> (Style, &'static str) {
    use crate::llm::ToolStatus;
    match status {
        ToolStatus::Success => (Style::bold(Color::Green), "✓"),
        ToolStatus::Error | ToolStatus::Unknown => (Style::bold(Color::Red), "×"),
        ToolStatus::Cancelled | ToolStatus::Skipped => (Style::new(Color::Dim), "⊘"),
    }
}

/// The text of a result to draw in the transcript, which is not always the text stored.
///
/// A cancelled or skipped call has no result. What it stores is written for the model — it
/// has to know the call did not complete and must check before retrying — and showing that
/// to the user puts an instruction addressed to the machine in front of a person who
/// pressed Esc and already knows. The stored message keeps it; the transcript does not.
fn result_text(status: crate::llm::ToolStatus, content: &str) -> &str {
    use crate::llm::ToolStatus;
    match status {
        ToolStatus::Success | ToolStatus::Error | ToolStatus::Unknown => content,
        ToolStatus::Cancelled | ToolStatus::Skipped => "",
    }
}

/// The display a call would have, derived from its arguments alone. Used while running,
/// when there is no result yet to describe it.
fn arguments_display(name: &str, arguments: &serde_json::Value) -> Display {
    match name {
        "bash" => Display::Command { footer: Vec::new() },
        "write" | "edit" | "read" | "grep" | "find" | "ls" => Display::File {
            verb: crate::tools::verb_for(name),
            path: arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string(),
        },
        _ => Display::None,
    }
}

/// Build a finished call with its header and outcome always visible.
pub fn tool_block(
    name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
    status: crate::llm::ToolStatus,
) -> Block {
    let mut lines: Vec<Line> = Vec::new();
    let mut head = 1usize;
    let mut tail = 0usize;
    let (mark_style, mark) = status_mark(status);
    let mark_span = Span::new(mark, mark_style);
    let body = result_text(status, &output.content);

    match &output.display {
        Display::Command { footer, .. } => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(name);
            lines.extend(command_header(&mark_span, command));
            // A header with no result under it is a complete statement — "this command, no
            // output" — so the blank separator only appears when something follows.
            if !body.is_empty() {
                lines.push(Line::blank());
                lines.extend(plain_text_lines(body, Style::plain()));
            }
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
                // The exit code and the temp-file path are written into the output as well as
                // into the footer, and both copies are right where they are: the model reads
                // the text, and the footer survives collapsing. The transcript is where the
                // two meet, and printing them twice puts the same path on two adjacent rows.
                // The output's copy wins — it is the one a replayed session still has.
                if output_states_note(&output.content, note) {
                    continue;
                }
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
            let mut rows = diff::render(&output.display);
            if !rows.is_empty() {
                lines.push(rows.remove(0));
                head = 2;
            }
            lines.extend(rows);
            // A change is read from both ends: what was taken away at the top, what replaced
            // it at the bottom. Command output is read from its tail only, which is why this
            // is a different block shape rather than a different preview length.
            let budget = Defaults::DIFF_PREVIEW_LINES;
            let half = budget / 2;
            return Block::collapsible_excerpted(lines, head, tail, half, budget - half);
        }
        Display::File { verb, path } => {
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(" "),
                Span::plain(*verb),
                Span::plain(" "),
                Span::new(util::one_line(path), Style::new(Color::Cyan)),
            ]));
            lines.extend(plain_text_lines(body, Style::new(Color::Output)));
        }
        Display::None => {
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(format!(" {name}")),
            ]));
            lines.extend(plain_text_lines(body, Style::plain()));
        }
    }
    Block::collapsible(lines, head, tail, Defaults::COMMAND_PREVIEW_LINES)
}

/// Whether the command's own output already states this footer note.
///
/// The tools write the exit code and the location of a cut-off output into the result text
/// as well as into the footer. Comparing is done on the text without the surrounding
/// brackets the tools wrap it in (`[退出码 3]` against `退出码 3`), and a note the output
/// states in full — the truncation note carries the temp path the footer also names — counts
/// as stated. The output is the copy that keeps working after a resume, so it is the one
/// the transcript keeps.
fn output_states_note(output: &str, note: &str) -> bool {
    if note.is_empty() {
        return false;
    }
    output.lines().any(|line| {
        let line = line
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        line.contains(note)
    })
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
    let mut spans = vec![
        Span::new(
            mark.text.clone(),
            Style {
                bold: true,
                ..mark.style
            },
        ),
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

    let mut out: Vec<crate::ui::screen::Block> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        // The environment block is bookkeeping that happens to be a user message. Filtered
        // here rather than by the caller so no replay path can forget and print it as
        // something the user said.
        if crate::agent::r#loop::is_environment_block(message) {
            continue;
        }
        match message {
            Message::User { content } => {
                let text = message.text();
                if text.trim().is_empty()
                    && content.iter().all(|b| !matches!(b, MsgBlock::Text { .. }))
                {
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
            Message::Assistant {
                content,
                stop_reason,
            } => {
                // Call ids may be reused in later responses. Pair only this response's results.
                let results: std::collections::HashMap<_, _> = messages[index + 1..]
                    .iter()
                    .take_while(|message| matches!(message, Message::Tool { .. }))
                    .filter_map(|message| match message {
                        Message::Tool {
                            tool_call_id,
                            content,
                            status,
                            ..
                        } => Some((tool_call_id.as_str(), (content.as_str(), *status))),
                        _ => None,
                    })
                    .collect();
                let mut lines: Vec<Line> = Vec::new();
                let had_thinking = content
                    .iter()
                    .any(|b| matches!(b, MsgBlock::Thinking { .. }));
                if had_thinking {
                    push_lines(&mut out, thinking_done_lines());
                }
                let stopped = *stop_reason == Some(crate::llm::StopReason::Aborted);
                // Said once, on the first hosted call or citation, not once per result.
                let mut announced_search = false;
                for block in content {
                    match block {
                        MsgBlock::Text { text } => {
                            push_lines(&mut out, std::mem::take(&mut lines));
                            if !text.trim().is_empty() {
                                out.push(Block::markdown(text));
                            }
                        }
                        MsgBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            if !lines.is_empty() {
                                push_lines(&mut out, std::mem::take(&mut lines));
                            }
                            let (content, status) = results
                                .get(id.as_str())
                                .copied()
                                .unwrap_or(("", crate::llm::ToolStatus::Unknown));
                            // A tool call keeps its own block, so the output is collapsed
                            // on resume exactly as it was when it ran. The status comes from
                            // the record, which is what keeps a call the user stopped from
                            // rendering differently here than it did live.
                            let output = stored_output(name, arguments, content);
                            out.push(tool_block(name, arguments, &output, status));
                        }
                        MsgBlock::Hosted { .. } => {
                            if !announced_search {
                                announced_search = true;
                                lines.push(Line::new(
                                    "搜索了网页".to_string(),
                                    Style::new(Color::Dim),
                                ));
                            }
                        }
                        MsgBlock::Citation { url, title } => {
                            if !announced_search {
                                announced_search = true;
                                lines.push(Line::new(
                                    "搜索了网页".to_string(),
                                    Style::new(Color::Dim),
                                ));
                            }
                            lines.push(Line::new(
                                crate::llm::citation_line(title, url),
                                Style::new(Color::Dim),
                            ));
                        }
                        _ => {}
                    }
                }
                push_lines(&mut out, lines);
                // A resumed session has to say that the answer was cut short, or a stopped
                // turn reads as a model that simply trailed off — and the user has no way to
                // tell "I stopped this" from "it gave up".
                if stopped {
                    push_lines(&mut out, note_lines("已停止", Style::new(Color::Dim)));
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
        "bash" => Display::Command { footer: Vec::new() },
        "write" | "edit" | "read" | "grep" | "find" | "ls" => Display::File {
            verb: crate::tools::verb_for(name),
            path: arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(util::one_line)
                .unwrap_or_else(|| "…".into()),
        },
        _ => Display::None,
    };
    output
}

/// The one-line marker that replaces the live thinking preview once a turn ends.
///
/// It carries **no** trailing blank row: the answer follows immediately underneath, and an
/// empty row between them reads as a paragraph break the model did not write. Blocks are
/// separated by the transcript itself wherever that is wanted.
pub fn thinking_done_lines() -> Vec<Line> {
    vec![Line::new("思考完成", Style::new(Color::Dim))]
}

/// The single line for a call in flight: `● $ sleep 30`.
///
/// Only the first line of a multi-line command, matching the collapsed transcript row, so
/// the running line and the finished one line up as the same thing.
pub fn running_line(name: &str, arguments: &serde_json::Value) -> Vec<Span> {
    let display = arguments_display(name, arguments);
    let (style, mark) = running_mark();
    let header = Span::new(mark, style);
    match &display {
        Display::Command { .. } => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(name);
            // A multi-line command collapses to its first line here too, so the running row
            // and the finished header are the same shape.
            let header_line = command_header(&header, command);
            header_line
                .into_iter()
                .next()
                .map(|line| line.spans)
                .unwrap_or_default()
        }
        Display::File { verb, path } => {
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
        "if", "then", "else", "elif", "fi", "for", "while", "do", "done", "case", "esac", "in",
        "function", "return", "echo", "cd", "export", "source", "set", "unset", "local",
        "readonly", "shift", "trap", "exit", "test", "git", "cargo", "rg", "fd", "grep", "find",
        "sed", "awk", "cat", "ls", "rm", "mv", "cp", "mkdir", "touch", "npm", "bun", "node",
        "python3", "curl", "wget",
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
    use crate::llm::ToolStatus;

    #[test]
    fn replay_keeps_each_responses_status_even_when_call_ids_repeat() {
        use crate::llm::{Block as MsgBlock, Message, StopReason, ToolStatus};
        let call = || Message::Assistant {
            content: vec![MsgBlock::ToolCall {
                id: "same".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"a.rs"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
        };
        let messages = vec![
            call(),
            Message::Tool {
                tool_call_id: "same".into(),
                name: "read".into(),
                content: "original data".into(),
                status: ToolStatus::Success,
            },
            Message::user_text("再读一次"),
            call(),
            Message::Tool {
                tool_call_id: "same".into(),
                name: "read".into(),
                content: "read failed".into(),
                status: ToolStatus::Error,
            },
        ];
        crate::llm::validate_tool_history(&messages).unwrap();
        let rendered: Vec<_> = replay_blocks(&messages)
            .iter()
            .map(|block| plain(&block.render(80)).join("\n"))
            .collect();
        assert!(rendered[0].contains("✓ 读取 a.rs"), "{}", rendered[0]);
        assert!(rendered[0].contains("original data"));
        assert!(!rendered[0].contains("read failed"));
        let last = rendered.last().unwrap();
        assert!(last.contains("× 读取 a.rs"), "{last}");
        assert!(last.contains("read failed"));
    }
    use crate::ui::plain;
    use crate::ui::screen::Bg;

    #[test]
    fn a_stopped_answer_says_so_when_it_is_replayed() {
        use crate::llm::{Block as MsgBlock, Message, StopReason};
        // Without the marker a resumed session shows a truncated reply with nothing to
        // explain it: the user cannot tell "I pressed Esc" from "the model trailed off".
        let stopped = vec![Message::Assistant {
            content: vec![MsgBlock::Text {
                text: "说到一半".into(),
            }],
            stop_reason: Some(StopReason::Aborted),
        }];
        let text: String = replay_blocks(&stopped)
            .iter()
            .flat_map(|block| plain(&block.render(80)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("说到一半"), "{text}");
        assert!(text.contains("已停止"), "{text}");

        // A normal answer carries no such note.
        let finished = vec![Message::Assistant {
            content: vec![MsgBlock::Text {
                text: "说完了".into(),
            }],
            stop_reason: Some(StopReason::Stop),
        }];
        let text: String = replay_blocks(&finished)
            .iter()
            .flat_map(|block| plain(&block.render(80)))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("已停止"), "{text}");
    }

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
                    MsgBlock::Thinking {
                        thinking: "先跑命令".into(),
                        signature: None,
                    },
                    MsgBlock::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
                    },
                ],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool {
                status: crate::llm::ToolStatus::Success,
                tool_call_id: "c1".into(),
                name: "bash".into(),
                content: "hi\n".into(),
            },
            Message::Assistant {
                content: vec![MsgBlock::Text {
                    text: "输出是 hi".into(),
                }],
                stop_reason: Some(StopReason::Stop),
            },
        ];
        let blocks: Vec<Vec<String>> = replay_blocks(&messages)
            .iter()
            .map(|block| plain(&block.render(80)))
            .collect();
        let text = blocks.join(&"".to_string()).join("\n");

        assert!(text.contains("› 跑一下 echo"), "{text}");
        assert!(
            text.contains("✓ $ echo hi"),
            "the command must come back: {text}"
        );
        assert!(
            text.contains("hi"),
            "the command's output must come back: {text}"
        );
        assert!(
            text.contains("输出是 hi"),
            "the answer must come back: {text}"
        );
        assert!(
            text.contains("思考完成"),
            "thinking stays collapsed: {text}"
        );
        // The environment block is bookkeeping, not something the user said.
        assert!(!text.contains("<environment>"), "{text}");
    }

    #[test]
    fn a_resumed_interrupted_call_looks_like_it_did_when_it_stopped() {
        // The status lives in the session file, so the block can be rebuilt the same way it
        // was drawn live. Without that the resumed view fell back to the tool name and
        // printed the note written for the model, which is the one thing the live view had
        // just been taught not to show.
        use crate::llm::{Block as MsgBlock, Message, StopReason, ToolStatus};
        let model_facing = "用户中止了工具执行，可能已有部分效果；请先检查实际状态。";
        let messages = vec![
            Message::Assistant {
                content: vec![MsgBlock::ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "sleep 30"}),
                }],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool {
                status: ToolStatus::Cancelled,
                tool_call_id: "c1".into(),
                name: "bash".into(),
                content: model_facing.into(),
            },
            Message::Assistant {
                content: vec![MsgBlock::Text {
                    text: "说到一半".into(),
                }],
                stop_reason: Some(StopReason::Aborted),
            },
        ];
        let blocks: Vec<Vec<String>> = replay_blocks(&messages)
            .iter()
            .map(|block| plain(&block.render(80)))
            .collect();
        let text = blocks.join(&"".to_string()).join("\n");
        assert!(text.contains("⊘ $ sleep 30"), "{text}");
        assert!(!text.contains("可能已有部分效果"), "{text}");
        // The one line that does belong to the user: the turn did not finish.
        assert!(text.contains("已停止"), "{text}");
    }

    #[test]
    fn the_thinking_marker_is_not_followed_by_a_blank_row() {
        // The marker sits directly above the answer it precedes. A blank row between them
        // reads as a paragraph break the model never wrote — and it appeared on every single
        // turn that involved thinking, which is most of them.
        let lines = thinking_done_lines();
        assert_eq!(lines.len(), 1, "the marker is one row: {lines:?}");
        assert!(
            !lines.last().unwrap().is_empty(),
            "and that row has text in it"
        );
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
            display: Display::Command {
                footer: vec!["耗时 0.4s".into()],
            },
            is_error: false,
            duration: None,
        };
        let block = tool_block(
            "bash",
            &serde_json::json!({"command": "ls -l"}),
            &output,
            ToolStatus::Success,
        );
        assert!(block.is_collapsible());
        let text = plain(&block.render(80));
        assert!(text[0].starts_with("✓ "), "{text:?}");
        assert!(text[0].contains("$ ls -l"), "{text:?}");
        // No row says "collapsed": the excerpt is the tail of the output, and the reader
        // does not need to be told that an excerpt is an excerpt on every tool call.
        assert!(!text.iter().any(|line| line.contains("收起")), "{text:?}");
        assert!(text.iter().any(|line| line.contains("row 19")), "{text:?}");
        // The footer note is the last row, visible even when collapsed.
        assert!(text.last().unwrap().contains("耗时 0.4s"), "{text:?}");
    }

    #[test]
    fn a_call_with_no_result_shows_what_was_attempted_and_not_the_note_for_the_model() {
        // Interrupting a command used to print three lines for one event, the middle one an
        // instruction addressed to the model ("check the state before retrying") that the
        // user is the last person who needs to read. The header names the call; the turn's
        // own `已停止` says the rest.
        let model_facing = "用户中止了工具执行，可能已有部分效果；请先检查实际状态。";
        let output = ToolOutput::error_for(
            "bash",
            &serde_json::json!({"command": "sleep 30"}),
            model_facing,
        );
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "sleep 30"}),
                &output,
                ToolStatus::Cancelled,
            )
            .render(80),
        );
        assert_eq!(text, vec!["⊘ $ sleep 30"], "{text:?}");
        assert!(
            !text.iter().any(|line| line.contains("检查实际状态")),
            "{text:?}"
        );

        // A refused or failed call is the opposite case: the text *is* the result, and it is
        // the only record of why the call did nothing.
        let refused = ToolOutput::error_for(
            "bash",
            &serde_json::json!({"command": "rm -rf /"}),
            "未获得用户授权，操作未执行（命令会删除文件，需要确认目标）",
        );
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "rm -rf /"}),
                &refused,
                ToolStatus::Error,
            )
            .render(80),
        );
        assert!(text[0].starts_with("× $ rm -rf /"), "{text:?}");
        assert!(
            text.iter().any(|line| line.contains("需要确认目标")),
            "{text:?}"
        );

        // A call that was never started stores a note for the model too, and the mark says
        // the one thing the user needs: it produced no result.
        let skipped = ToolOutput::error_for(
            "read",
            &serde_json::json!({"path": "src/main.rs"}),
            "用户停止了本轮，这个调用没有执行。",
        );
        let text = plain(
            &tool_block(
                "read",
                &serde_json::json!({"path": "src/main.rs"}),
                &skipped,
                ToolStatus::Skipped,
            )
            .render(80),
        );
        assert_eq!(text, vec!["⊘ 读取 src/main.rs"], "{text:?}");
    }

    #[test]
    fn a_note_the_output_already_states_is_not_printed_twice() {
        // A failed command used to read: `× $ false`, `[退出码 3]`, and then `退出码 3` again
        // on the next row — the same fact from the output text and from the footer. Both
        // copies belong where they are; the transcript is where they meet, so it shows one.
        let output = ToolOutput {
            content: "boom\n[退出码 3]\n".into(),
            display: Display::Command {
                footer: vec!["退出码 3".into()],
            },
            is_error: true,
            duration: None,
        };
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "false"}),
                &output,
                ToolStatus::Error,
            )
            .render(80),
        );
        assert_eq!(
            text.iter().filter(|line| line.contains("退出码")).count(),
            1,
            "{text:?}"
        );
        assert!(text[0].starts_with("× $ false"), "{text:?}");
        assert_eq!(text.last().unwrap(), "[退出码 3]", "{text:?}");

        // Same for the temp file a cut-off output is parked in: the truncation note at the
        // end of the output names it, and the footer used to name it again underneath.
        let huge: String = (0..40_000).map(|i| format!("row {i}\n")).collect();
        let output = ToolOutput {
            content: huge,
            display: Display::Command { footer: Vec::new() },
            is_error: false,
            duration: None,
        }
        .budget();
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "seq"}),
                &output,
                ToolStatus::Success,
            )
            .render(80),
        );
        assert_eq!(
            text.iter().filter(|line| line.contains("完整输出")).count(),
            1,
            "{text:?}"
        );

        // A note the output never mentions is the only copy there is, so it stays.
        let output = ToolOutput {
            content: "done\n".into(),
            display: Display::Command {
                footer: vec!["退出码 3".into()],
            },
            is_error: true,
            duration: None,
        };
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "false"}),
                &output,
                ToolStatus::Error,
            )
            .render(80),
        );
        assert!(text.iter().any(|line| line == "退出码 3"), "{text:?}");
    }

    #[test]
    fn a_running_call_is_marked_as_unfinished() {
        // A tick is a claim about the outcome, and a call that has not finished has no
        // outcome. Showing one while the command runs would say it succeeded before it did.
        let (style, mark) = running_mark();
        assert_eq!(mark, "●");
        assert!(
            !style.bold,
            "a running mark is not a verdict, so it is not bold"
        );

        // Settled calls keep the verdict marks.
        assert_eq!(status_mark(ToolStatus::Success).1, "✓");
        assert_eq!(status_mark(ToolStatus::Error).1, "×");
        assert_eq!(status_mark(ToolStatus::Unknown).1, "×");
        assert!(status_mark(ToolStatus::Success).0.bold);
        assert!(status_mark(ToolStatus::Error).0.bold);

        // Nothing was executed, so nothing went wrong: `×` would be a false verdict.
        assert_eq!(status_mark(ToolStatus::Cancelled).1, "⊘");
        assert_eq!(status_mark(ToolStatus::Skipped).1, "⊘");
        assert!(!status_mark(ToolStatus::Cancelled).0.bold);

        // The live line names the command, and a multi-line one collapses to its first line
        // so it matches the header that replaces it.
        let line = running_line("bash", &serde_json::json!({"command": "ls -l\necho done"}));
        let text: String = line.iter().map(|span| span.text.as_str()).collect();
        assert!(text.starts_with("● $ ls -l"), "{text:?}");
        assert!(
            !text.contains("echo done"),
            "only the first line is shown: {text:?}"
        );
    }

    #[test]
    fn a_command_result_reports_how_long_it_took() {
        // The duration is a property of the call, and the collapsed view is the one the user
        // compares between runs, so it has to survive collapsing.
        let mut output = ToolOutput {
            content: "done\n".into(),
            display: Display::Command { footer: Vec::new() },
            is_error: false,
            duration: None,
        };
        let block = tool_block(
            "bash",
            &serde_json::json!({"command": "sleep 1"}),
            &output,
            ToolStatus::Success,
        );
        let text = plain(&block.render(80));
        assert!(
            !text.iter().any(|line| line.contains("耗时")),
            "no time, no claim"
        );

        output.duration = Some(std::time::Duration::from_millis(3400));
        let block = tool_block(
            "bash",
            &serde_json::json!({"command": "sleep 1"}),
            &output,
            ToolStatus::Success,
        );
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
            display: Display::Command {
                footer: vec!["退出码 3".into()],
            },
            is_error: true,
            duration: None,
        };
        let text = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "false"}),
                &output,
                ToolStatus::Error,
            )
            .render(80),
        );
        assert!(text[0].starts_with("× "), "{text:?}");
        assert!(
            text.iter().any(|line| line.contains("退出码 3")),
            "{text:?}"
        );
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
            let joined: String = highlight_spans(command)
                .iter()
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(joined, command, "highlighting changed {command:?}");
        }
    }

    #[test]
    fn keywords_and_comments_get_distinct_styles() {
        let spans = highlight_spans("git log # why");
        assert_eq!(spans[0].text, "git");
        assert_eq!(spans[0].style.fg, Color::Output);
        assert!(
            spans
                .iter()
                .any(|s| s.text == "# why" && s.style.fg == Color::Dim)
        );
    }

    #[test]
    fn a_diff_block_keeps_red_and_green_rows() {
        let output = ToolOutput {
            content: "已修改 a.rs".into(),
            display: crate::ui::diff::for_edit("one\ntwo\n", "one\nTWO\n"),
            is_error: false,
            duration: None,
        };
        let block = tool_block(
            "edit",
            &serde_json::json!({"path": "a.rs"}),
            &output,
            ToolStatus::Success,
        );
        // The header and the +/- summary stay visible when collapsed.
        let collapsed = plain(&block.render(80));
        assert!(collapsed[0].contains("修改 a.rs"), "{collapsed:?}");
        assert!(
            collapsed[1].contains("+1") && collapsed[1].contains("−1"),
            "{collapsed:?}"
        );
        // A diff this small fits the preview, so nothing is hidden and there is no note.
        assert!(
            !collapsed.iter().any(|line| line.contains("已收起")),
            "{collapsed:?}"
        );
        // Rows carry the codex tints. The row text is `-    2 │ two`, so match on the payload.
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
    fn a_diff_taller_than_the_preview_shows_an_excerpt_of_itself() {
        let before: String = (0..40).map(|i| format!("old {i}\n")).collect();
        let after: String = (0..40).map(|i| format!("new {i}\n")).collect();
        let output = ToolOutput {
            content: "已修改 big.rs".into(),
            display: crate::ui::diff::for_edit(&before, &after),
            is_error: false,
            duration: None,
        };
        let text = plain(
            &tool_block(
                "edit",
                &serde_json::json!({"path": "big.rs"}),
                &output,
                ToolStatus::Success,
            )
            .render(80),
        );
        // The header and the counts are the first two rows, and real changed rows follow —
        // not a sentence standing in for them.
        assert!(text[0].contains("修改 big.rs"), "{text:?}");
        assert!(
            text[1].contains("+40") && text[1].contains("−40"),
            "{text:?}"
        );
        assert!(
            text[2..].iter().any(|line| line.contains("old 0")),
            "{text:?}"
        );
        assert!(
            text[2..].iter().any(|line| line.contains("new 39")),
            "{text:?}"
        );
        assert!(!text.iter().any(|line| line.contains("收起")), "{text:?}");
    }

    #[test]
    fn a_file_result_shows_a_header_and_a_short_preview() {
        let output = ToolOutput {
            content: (0..20).map(|i| format!("entry {i}\n")).collect(),
            display: Display::File {
                verb: "列出",
                path: "src".into(),
            },
            is_error: false,
            duration: None,
        };
        let text = plain(
            &tool_block("ls", &serde_json::json!({}), &output, ToolStatus::Success).render(120),
        );
        assert!(
            text.iter().any(|line| line.contains("列出 src")),
            "{text:?}"
        );
        // The tail of the listing is what survives, with nothing announcing the cut.
        assert!(
            text.iter().any(|line| line.contains("entry 19")),
            "{text:?}"
        );
        assert!(!text.iter().any(|line| line.contains("收起")), "{text:?}");
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
        let text = plain(
            &tool_block("read", &serde_json::json!({}), &output, ToolStatus::Success).render(80),
        );
        assert!(
            text.iter()
                .all(|line| !line.contains('\u{1b}') && !line.contains('\u{7}'))
        );
    }

    #[test]
    fn every_line_contains_no_escape_sequences() {
        let output = ToolOutput {
            content: "ok\n".into(),
            display: Display::Command {
                footer: vec!["退出码 1".into()],
            },
            is_error: true,
            duration: None,
        };
        for line in tool_block(
            "bash",
            &serde_json::json!({"command": "false"}),
            &output,
            ToolStatus::Error,
        )
        .render(60)
        {
            assert!(!line.text().contains('\u{1b}'), "{line:?}");
        }
    }
}
