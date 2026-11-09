//! Turning tool results into compact transcript blocks.
//!
//! Compact transcript shapes; Ctrl+O opens their full content in a temporary view:
//!
//! * commands — one heading and a short, indented output excerpt,
//! * a file diff — red and green with line numbers, previewed at 14 rows,
//! * failures — a header and an excerpt from both ends of the error.
//!
//! Text is sanitized before it becomes a block, so nothing a command printed can move the
//! cursor or recolour the UI, and styling is expressed as spans rather than escape codes.

use std::borrow::Cow;

use crate::config::Defaults;
use crate::tools::{Display, ToolOutput};
use crate::ui::diff;
use crate::ui::screen::{Block, Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

#[cfg(test)]
mod output_tests;

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
/// A cancelled, skipped or unknown call has no result of its own. What it stores is written
/// for the model — it has to know the call did not complete and must check before retrying —
/// and showing that to the user puts an instruction addressed to the machine in front of a
/// person who pressed Esc or whose last session died, both of which they already know. The
/// stored message keeps it; the transcript does not. `Unknown` was the one status left out of
/// this list, so an interrupted call printed `请先检查文件或实际状态，再决定是否重试。`
/// under a `×` — advice for the model, in the user's transcript.
pub(super) fn result_text(status: crate::llm::ToolStatus, content: &str) -> Cow<'_, str> {
    use crate::llm::ToolStatus;
    match status {
        ToolStatus::Success | ToolStatus::Error => {
            if content.trim() == crate::tools::NO_OUTPUT {
                return Cow::Borrowed("");
            }
            if !content.lines().any(model_output_note) {
                return Cow::Borrowed(content);
            }
            Cow::Owned(
                content
                    .lines()
                    .filter(|line| !model_output_note(line))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim_end()
                    .to_string(),
            )
        }
        ToolStatus::Cancelled | ToolStatus::Skipped | ToolStatus::Unknown => Cow::Borrowed(""),
    }
}

/// Recognize only pi's framed output annotations, never an arbitrary mention of truncation.
/// The original result stays in the session and model request. Both live and restored
/// transcripts use this projection, including sessions written before the UI changed.
fn model_output_note(line: &str) -> bool {
    let Some(note) = line
        .strip_prefix('[')
        .and_then(|line| line.strip_suffix(']'))
    else {
        return false;
    };
    if note.starts_with("输出过长，已截断。完整输出：")
        || note.starts_with("完整输出写入临时文件失败：")
        || (note.starts_with("工具结果中间已裁剪；原始内容保存在 ")
            && note.contains("，tool_call_id="))
    {
        return true;
    }
    [
        ("已截断，仅返回前 ", " 条匹配"),
        ("已截断，仅列出前 ", " 个"),
    ]
    .iter()
    .any(|(prefix, suffix)| {
        note.strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            .is_some_and(|count| count.parse::<u64>().is_ok())
    })
}

/// The display a call would have, derived from its arguments alone.
///
/// This is the one place that decides how a call is described when all there is to go on is
/// what the model asked for: the running line uses it before there is a result, and a
/// resumed session uses it after one — the display itself is presentation and is not stored.
/// Keeping it in one place is what makes those two views agree; three copies of the same
/// match is how a live `● 搜索 .` came back from a resume as `✓ 搜索 …`.
///
/// The default for a missing `path` is the tool's own: the search tools work on the working
/// directory when none is given, while the file tools require one and a call without it is
/// better shown as an unnamed target than as the tool's name.
pub(crate) fn arguments_display(name: &str, arguments: &serde_json::Value) -> Display {
    let path = |fallback: &str| {
        if let Some(label) = batch_path_label(arguments) {
            return label;
        }
        arguments
            .get("path")
            .and_then(|value| value.as_str())
            .map(util::one_line)
            .unwrap_or_else(|| fallback.to_string())
    };
    match name {
        "bash" => Display::Command,
        "grep" | "find" | "ls" => Display::File {
            verb: crate::tools::verb_for(name),
            path: path("."),
        },
        "read" | "read_image" | "write" | "edit" => Display::File {
            verb: crate::tools::verb_for(name),
            path: path("…"),
        },
        _ => Display::None,
    }
}

/// Keep batch headings compact; the result body contains every complete target path.
fn batch_path_label(arguments: &serde_json::Value) -> Option<String> {
    arguments.get("path")?.as_array()?;
    let paths = crate::tools::input_paths(arguments).ok()?;
    let shown = paths
        .iter()
        .take(2)
        .map(|path| util::truncate(&util::one_line(path), 24, "…"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(if paths.len() > 2 {
        format!("{shown} 等 {} 个路径", paths.len())
    } else {
        shown
    })
}

/// Build a finished call with its header and outcome always visible.
pub fn tool_block(
    name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
    status: crate::llm::ToolStatus,
) -> Block {
    let mut block = tool_block_inner(name, arguments, output, status);
    if let Block::Collapsible(inner) = &mut block {
        inner.full_output = output.content.lines().rev().find_map(util::output_log_path);
        inner.tool_output = true;
    }
    block
}

fn tool_block_inner(
    name: &str,
    arguments: &serde_json::Value,
    output: &ToolOutput,
    status: crate::llm::ToolStatus,
) -> Block {
    let mut lines: Vec<Line> = Vec::new();
    let mut head = 1usize;
    let tail = 0usize;
    let (mark_style, mark) = status_mark(status);
    let mark_span = Span::new(mark, mark_style);
    let body = result_text(status, &output.content);

    match &output.display {
        Display::Command => {
            let command = arguments
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or(name);
            let mut header = command_header(&mark_span, command);
            // Where the command ran, when that is not the session's own directory. Without
            // it the block is only true for the default case, and a call that ran elsewhere
            // reads as if it had run here — the relative paths in its output mean otherwise.
            if let Some(cwd) = arguments.get("cwd").and_then(|v| v.as_str())
                && !cwd.trim().is_empty()
                && let Some(line) = header.last_mut()
            {
                line.spans.push(Span::new(
                    format!("（在 {}）", util::one_line(cwd)),
                    Style::new(Color::Dim),
                ));
            }
            if let Some(duration) = output.duration
                && let Some(line) = header.last_mut()
            {
                line.spans.push(Span::new(
                    format!(" · {}", format_duration(duration)),
                    Style::new(Color::Dim),
                ));
            }
            lines.extend(header);
            if !body.is_empty() {
                lines.extend(plain_text_lines(&body, Style::new(Color::Output)));
            }
        }
        Display::Diff { .. } => {
            // The verb comes from the call, not from the block shape: `write` and `edit` both
            // render a diff, and a hardcoded 修改 made a live write read differently from the
            // same write on the next resume.
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(" "),
                Span::plain(crate::tools::verb_for(name)),
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
            // Keep both ends of the change visible, including removed and added lines.
            let budget = Defaults::DIFF_PREVIEW_LINES;
            let half = budget / 2;
            return Block::collapsible_excerpted(lines, head, tail, half, budget - half);
        }
        Display::File { verb, path } => {
            let batch_path = batch_path_label(arguments);
            let mut header = vec![
                mark_span.clone(),
                Span::plain(" "),
                Span::plain(*verb),
                Span::plain(" "),
                Span::new(
                    batch_path.clone().unwrap_or_else(|| util::one_line(path)),
                    Style::new(Color::Cyan),
                ),
            ];
            if status == crate::llm::ToolStatus::Success {
                let summary = match body.trim() {
                    _ if batch_path.is_some() => String::new(),
                    "没有找到匹配。" | "没有找到匹配的文件。" => {
                        "无匹配".to_string()
                    }
                    "" => String::new(),
                    _ if name == "read" && !output.content.lines().any(model_output_note) => {
                        format!("{} 行", body.lines().count())
                    }
                    _ => String::new(),
                };
                if !summary.is_empty() {
                    header.push(Span::new(format!(" · {summary}"), Style::new(Color::Dim)));
                }
            }
            lines.push(Line::spans(header));
            lines.extend(plain_text_lines(&body, Style::new(Color::Output)));
        }
        Display::None => {
            lines.push(Line::spans(vec![
                mark_span.clone(),
                Span::plain(format!(" {name}")),
            ]));
            lines.extend(plain_text_lines(&body, Style::plain()));
        }
    }
    if status == crate::llm::ToolStatus::Error {
        // Keep the cause at the start and the exit status at the end of long errors.
        let budget = Defaults::ERROR_PREVIEW_LINES;
        Block::collapsible_excerpted(lines, head, tail, budget / 2, budget - budget / 2)
    } else if matches!(output.display, Display::Command) {
        Block::collapsible_excerpted(lines, head, tail, 3, 0)
    } else {
        Block::collapsible(lines, head, tail, 0)
    }
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
/// Adjacent notes share a block so replay does not add extra separators.
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
    output.display = arguments_display(name, arguments);
    output
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
        Display::Command => {
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
                images: Vec::new(),
            },
            Message::user_text("再读一次"),
            call(),
            Message::Tool {
                tool_call_id: "same".into(),
                name: "read".into(),
                content: "read failed".into(),
                status: ToolStatus::Error,
                images: Vec::new(),
            },
        ];
        crate::llm::validate_tool_history(&messages).unwrap();
        let rendered: Vec<_> = replay_blocks(&messages)
            .into_iter()
            .map(|mut block| {
                if let Block::Collapsible(inner) = &mut block {
                    inner.expanded = true;
                }
                plain(&block.render(80)).join("\n")
            })
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
                    MsgBlock::Text {
                        text: "先确认命令输出".into(),
                    },
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
                images: Vec::new(),
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
        let replayed = replay_blocks(&messages);
        let blocks: Vec<Vec<String>> = replayed
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
            !text.contains("思考完成"),
            "thinking leaves no permanent marker: {text}"
        );
        // The environment block is bookkeeping, not something the user said.
        assert!(!text.contains("<environment>"), "{text}");
        assert_eq!(text.matches("先确认命令输出").count(), 1);
        assert!(text.find("先确认命令输出").unwrap() < text.find("✓ $ echo hi").unwrap());
        let mut detail = replayed
            .iter()
            .find(|block| block.is_collapsible())
            .unwrap()
            .clone();
        if let Block::Collapsible(inner) = &mut detail {
            inner.expanded = true;
        }
        let detail = plain(&detail.render(80)).join("\n");
        assert!(!detail.contains("先确认命令输出"));
        assert!(detail.contains("hi"));
    }

    #[test]
    fn an_interrupted_call_does_not_print_the_advice_written_for_the_model() {
        // `Unknown` is what a session that died mid-call leaves behind. Its stored text is
        // addressed to the model ("请先检查文件或实际状态，再决定是否重试"), and that used to
        // reach the transcript under a `×`, because this status was left out of the list of
        // statuses with no result to show. Live and resumed agree, as they must.
        use crate::llm::ToolStatus;
        let model_facing =
            "上次会话意外中断，此工具调用的执行结果未知。请先检查文件或实际状态，再决定是否重试。";
        let mut output = ToolOutput::text(model_facing);
        output.display = arguments_display("bash", &serde_json::json!({"command": "cargo build"}));
        let rendered = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "cargo build"}),
                &output,
                ToolStatus::Unknown,
            )
            .render(80),
        );
        let text = rendered.join("\n");
        assert!(text.contains("cargo build"), "{text}");
        assert!(!text.contains("请先检查"), "{text}");
        assert!(!text.contains("重试"), "{text}");
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
                images: Vec::new(),
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

    /// 一次调用的头在实时与恢复会话两条路上必须一致。
    ///
    /// 显示层不进会话文件，所以恢复会话时它由参数重新推出——这正是两处会悄悄分叉的
    /// 地方：曾经实时是 `× read`（丢了调用描述）、恢复是 `× 读取 nope.rs`；`write` 的
    /// diff 头写死「修改」，恢复却按调用说「写入」；搜索不传 path 时实时是 `.`、恢复是 `…`。
    #[test]
    fn a_call_reads_the_same_live_and_after_a_resume() {
        use crate::llm::{Block as MB, Message, StopReason};
        use crate::tools::ToolOutput;

        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("bash", serde_json::json!({"command": "ls -l"})),
            (
                "bash",
                serde_json::json!({"command": "ls -l", "cwd": "/tmp"}),
            ),
            ("bash", serde_json::json!({"command": "ls -l", "cwd": "  "})),
            ("read", serde_json::json!({"path": "src/a.rs"})),
            ("read", serde_json::json!({"path": "nope.rs"})),
            (
                "read",
                serde_json::json!({"path": ["src/a.rs", "src/b.rs"]}),
            ),
            (
                "grep",
                serde_json::json!({"pattern":"fn", "path":(0..16).map(|n| format!("src/很长的项目目录/{}/file-{n}.rs", "nested/".repeat(10))).collect::<Vec<_>>()}),
            ),
            ("write", serde_json::json!({"path": "a.rs", "content": "x"})),
            ("edit", serde_json::json!({"path": "a.rs"})),
            ("grep", serde_json::json!({"pattern": "fn"})),
            ("grep", serde_json::json!({"pattern": "fn", "path": "src"})),
            ("find", serde_json::json!({"pattern": "*.rs"})),
            ("ls", serde_json::json!({})),
            ("ls", serde_json::json!({"path": "src"})),
        ];
        for (name, arguments) in cases {
            let mut output = ToolOutput::text("body\n");
            output.display = arguments_display(name, &arguments);
            if let Some(paths) = arguments.get("path").and_then(serde_json::Value::as_array) {
                let paths: Vec<_> = paths.iter().map(|path| path.as_str().unwrap()).collect();
                output.display = Display::File {
                    verb: crate::tools::verb_for(name),
                    path: paths.join(", "),
                };
                output.content = paths
                    .iter()
                    .map(|path| format!("── {path}\nbody\n"))
                    .collect();
                let running = running_line(name, &arguments)
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>();
                assert!(util::width(&running) <= 80, "{running}");
                let mut detail = tool_block(name, &arguments, &output, ToolStatus::Success);
                if let Block::Collapsible(inner) = &mut detail {
                    inner.expanded = true;
                }
                let detail = plain(&detail.render(1000)).join("\n");
                for path in paths {
                    assert!(detail.contains(path), "{path}");
                }
            }
            for status in [
                ToolStatus::Success,
                ToolStatus::Error,
                ToolStatus::Cancelled,
                ToolStatus::Skipped,
            ] {
                if status != ToolStatus::Success {
                    output.is_error = true;
                }
                let live = plain(&tool_block(name, &arguments, &output, status).render(80));
                let resumed: Vec<String> = replay_blocks(&[
                    Message::Assistant {
                        content: vec![MB::ToolCall {
                            id: "c1".into(),
                            name: name.into(),
                            arguments: arguments.clone(),
                        }],
                        stop_reason: Some(StopReason::ToolUse),
                    },
                    Message::Tool {
                        status,
                        tool_call_id: "c1".into(),
                        name: name.into(),
                        content: output.content.clone(),
                        images: Vec::new(),
                    },
                ])
                .iter()
                .flat_map(|block| plain(&block.render(80)))
                .filter(|line| !line.is_empty())
                .collect();
                assert_eq!(
                    live.first(),
                    resumed.first(),
                    "{name} {arguments} {status:?} 的头在两条路上不一致"
                );
                if arguments
                    .get("path")
                    .is_some_and(serde_json::Value::is_array)
                {
                    assert!(util::width(live.first().unwrap()) <= 80);
                }
            }
        }
    }

    /// 工具结果的正文不复述 header 已经写着的路径。
    #[tokio::test]
    async fn a_tool_result_never_repeats_the_path_in_its_header() {
        let dir = std::env::temp_dir().join(format!("pi-dup-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let context = crate::tools::ToolContext {
            cwd: dir.clone(),
            shell: "/usr/bin/zsh".into(),
            observed: Default::default(),
            progress: None,
        };
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("read", serde_json::json!({"path": "nope.rs"})),
            ("read", serde_json::json!({"path": "."})),
            (
                "write",
                serde_json::json!({"path": "new.rs", "content": "x"}),
            ),
            (
                "edit",
                serde_json::json!({"path": "a.rs", "old_text": "zzz", "new_text": "y"}),
            ),
            ("ls", serde_json::json!({})),
            ("grep", serde_json::json!({"pattern": "zzz"})),
            ("find", serde_json::json!({"pattern": "*.zzz"})),
        ];
        for (name, arguments) in cases {
            let output = crate::tools::execute(name, &arguments, &context).await;
            let status = if output.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
            // 路径在参数里、在 header 里，不该再出现在正文里。
            if let Some(path) = arguments.get("path").and_then(|value| value.as_str()) {
                let lines = plain(&tool_block(name, &arguments, &output, status).render(80));
                let mentions = lines.iter().filter(|line| line.contains(path)).count();
                assert_eq!(mentions, 1, "{name} {arguments} 把路径写了两遍：{lines:?}");
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_successful_command_shows_a_short_excerpt_and_expands_to_full_output() {
        let output = ToolOutput {
            content: (0..20).map(|i| format!("row {i}\n")).collect(),
            display: Display::Command,
            is_error: false,
            duration: Some(std::time::Duration::from_millis(400)),
            images: Vec::new(),
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
        assert_eq!(
            text,
            [
                "✓ $ ls -l · 0.4s",
                "  └ row 0",
                "    row 1",
                "    row 2",
                "    + 17 行 · Ctrl+O 查看",
                ""
            ]
        );
        let Block::Collapsible(mut expanded) = block else {
            unreachable!()
        };
        expanded.expanded = true;
        let full = plain(&Block::Collapsible(expanded).render(80));
        assert!(full.contains(&"row 0".into()));
        assert!(full.contains(&"row 19".into()));
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
        assert_eq!(text, vec!["⊘ $ sleep 30", ""], "{text:?}");
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
        assert_eq!(text, vec!["⊘ 读取 src/main.rs", ""], "{text:?}");
    }

    #[test]
    fn command_status_is_shown_once_and_truncation_is_model_only() {
        // Exit status stays in the result text, which both live display and replay use.
        let output = ToolOutput {
            content: "boom\n[退出码 3]\n".into(),
            display: Display::Command,
            is_error: true,
            duration: None,
            images: Vec::new(),
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
        assert_eq!(
            text.iter()
                .rev()
                .find(|line| !line.is_empty())
                .unwrap()
                .trim(),
            "[退出码 3]",
            "{text:?}"
        );

        // Truncation and the full-log path belong only in the model's result.
        let huge: String = (0..40_000).map(|i| format!("row {i}\n")).collect();
        let output = ToolOutput {
            content: huge,
            display: Display::Command,
            is_error: false,
            duration: None,
            images: Vec::new(),
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
            0,
            "{text:?}"
        );
        assert!(output.content.contains("输出过长，已截断。完整输出："));
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
            display: Display::Command,
            is_error: true,
            duration: None,
            images: Vec::new(),
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
    fn a_command_that_ran_elsewhere_says_where() {
        // `cwd` decides what every relative path in the output means, so a block that
        // omitted it would be describing a different command than the one ran.
        let output_with = |arguments: &serde_json::Value| {
            let mut output = ToolOutput::text("ok\n");
            output.display = arguments_display("bash", arguments);
            output
        };
        let here = plain(
            &tool_block(
                "bash",
                &serde_json::json!({"command": "ls"}),
                &output_with(&serde_json::json!({"command": "ls"})),
                ToolStatus::Success,
            )
            .render(80),
        );
        assert!(!here.join("\n").contains("（在 "), "{here:?}");

        let args = serde_json::json!({"command": "ls", "cwd": "/tmp"});
        let elsewhere =
            plain(&tool_block("bash", &args, &output_with(&args), ToolStatus::Success).render(80));
        assert!(elsewhere[0].contains("（在 /tmp）"), "{elsewhere:?}");
    }

    #[test]
    fn a_blank_cwd_is_not_shown_as_a_directory() {
        // An empty value means "the session's directory", not "some directory called space".
        let args = serde_json::json!({"command": "ls", "cwd": "   "});
        let mut output = ToolOutput::text("ok\n");
        output.display = arguments_display("bash", &args);
        let rendered = plain(&tool_block("bash", &args, &output, ToolStatus::Success).render(80));
        assert!(!rendered[0].contains("（在 "), "{rendered:?}");
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
            images: Vec::new(),
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
        assert!(
            removed
                .spans
                .iter()
                .any(|span| span.text.contains("two") && span.style.bg == Bg::Removed)
        );
        let added = painted
            .iter()
            .find(|line| line.text().contains("TWO"))
            .unwrap_or_else(|| panic!("the added row is missing: {painted:?}"));
        assert!(
            added
                .spans
                .iter()
                .any(|span| span.text.contains("TWO") && span.style.bg == Bg::Added)
        );
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
            images: Vec::new(),
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
    fn control_characters_in_tool_output_are_neutralised() {
        let output = ToolOutput {
            content: "safe\u{7}bell\u{1b}[2J".into(),
            display: Display::None,
            is_error: false,
            duration: None,
            images: Vec::new(),
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
            display: Display::Command,
            is_error: true,
            duration: None,
            images: Vec::new(),
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
