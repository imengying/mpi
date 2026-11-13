//! Behaviour of the screen, asserted on the values it produces.

use super::*;
use crate::ui::footer::FooterState;

fn drain_queued(screen: &mut Screen) -> Vec<Queued> {
    std::iter::from_fn(|| screen.pop_queued()).collect()
}

#[test]
fn editing_a_queued_message_withdraws_it_and_restores_text_and_images() {
    let mut screen = screen_with_commands();
    screen.interactive = false;
    set_input(&mut screen, "");
    let image = PastedImage {
        width: 1,
        height: 1,
        data: "png".into(),
        media_type: "image/png",
        bytes: 3,
    };
    let text = "long pasted line\n".repeat(50);
    screen.queue(Queued::new("older".into(), Vec::new()));
    screen.queue(Queued::new(text.clone(), vec![image.clone()]));
    let live: String = screen
        .compose_live()
        .0
        .iter()
        .map(|row| row.line.text())
        .collect();
    assert!(live.contains("待处理") && live.contains("Alt+↑"));
    screen.absorb_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));
    assert_eq!(screen.editing.as_ref().unwrap().expanded(), text);
    assert_eq!(
        screen.pending_images.as_slice(),
        std::slice::from_ref(&image)
    );
    assert_eq!(
        screen.take_steering(),
        [Queued::new("older".into(), Vec::new())]
    );
    assert!(screen.take_steering().is_empty());
    assert_eq!(
        screen.absorb_event(Event::Key(KeyCode::Enter.into())),
        Some(Action::LineWithImages(text, vec![image]))
    );
    assert!(screen.pending_images.is_empty());
}

#[test]
fn queued_recall_preserves_an_existing_draft_and_its_images() {
    let mut screen = screen_with_commands();
    screen.interactive = false;
    screen.queue(Queued::new("pending".into(), Vec::new()));
    set_input(&mut screen, "draft");
    let recall = Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    screen.absorb_event(recall.clone());
    assert_eq!(screen.editing.as_ref().unwrap().expanded(), "draft");
    assert_eq!(screen.pending.len(), 1);
    assert!(screen.notice.is_some());
    set_input(&mut screen, "");
    screen.pending_images.push(PastedImage {
        width: 1,
        height: 1,
        data: "draft".into(),
        media_type: "image/png",
        bytes: 5,
    });
    screen.absorb_event(recall);
    assert_eq!(screen.pending_images[0].data, "draft");
    assert_eq!(screen.pending.len(), 1);
}

#[tokio::test]
async fn commands_reveal_their_menus_and_feedback_after_tool_details() {
    let dir = std::env::temp_dir().join(format!("pi-queued-view-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&dir).unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "providers":[{"name":"test","api":"responses","base_url":"http://127.0.0.1:9/v1","api_key":"unused","models":[{"id":"m"}]}],
        "default_model":"test/m"
    })).unwrap();
    let mut agent = crate::agent::r#loop::Agent::new(config, dir.clone(), false).unwrap();
    agent.session = crate::agent::session::Session::create_in(&dir, &dir, "test/m").unwrap();
    agent.screen = screen();
    let captured = CapturedOutput::default();
    agent.screen.out = Box::new(captured.clone());
    agent.screen.interactive = true;
    set_input(&mut agent.screen, "draft");
    agent.screen.push(Block::collapsible(
        (0..60)
            .map(|n| Line::plain(format!("tool output {n}")))
            .collect(),
        1,
        0,
        0,
    ));
    agent.screen.flush();
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 100);
    {
        assert!(agent.screen.show_tool_details());
        agent.screen.flush();
        terminal.process(&captured.take());
        assert!(terminal.screen().contents().contains("工具详情"));
        assert!(agent.command("/model").await.unwrap());
        assert!(agent.screen.picker.is_some());
        agent.screen.begin_stream();
        agent.screen.push_text("background answer");
        agent.screen.end_stream();
        agent.screen.flush();
        terminal.process(&captured.take());
        assert!(terminal.screen().contents().contains("选择模型"));
        let action = agent.screen.absorb_event(Event::Key(KeyCode::Esc.into()));
        assert_eq!(action, Some(Action::Selection(None)));
        agent.handle_selection(None).unwrap();
        assert!(agent.command("/compact").await.unwrap());
        agent.screen.flush();
        terminal.process(&captured.take());
        let visible = terminal.screen().contents();
        assert!(visible.contains("历史太短"), "{visible}");
        assert!(visible.contains("› draft"), "{visible}");
        assert!(visible.contains("background answer"), "{visible}");
        assert!(!visible.contains("工具详情"), "{visible}");
    }
    drop(agent);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_error_exit_erases_the_composer_before_the_shell_resumes() {
    for approval in [false, true] {
        let captured = CapturedOutput::default();
        let mut terminal = vt100::Parser::new(nz(24), nz(80), 100);
        {
            let mut screen = screen();
            screen.width = 80;
            screen.out = Box::new(captured.clone());
            screen.interactive = true;
            screen.push_lines(vec![Line::plain("completed output")]);
            set_input(&mut screen, "unsent draft");
            screen.set_footer(vec![Line::plain("stats")]);
            screen.flush();
            terminal.process(&captured.take());
            assert!(terminal.screen().contents().contains("unsent draft"));
            if approval {
                screen.suspend_live();
                let (sender, _receiver) = tokio::sync::oneshot::channel();
                screen.approval = Some((
                    crate::ui::auth_panel::PanelState::new("APPROVAL_BODY"),
                    sender,
                ));
                screen.flush();
                terminal.process(&captured.take());
                assert!(terminal.screen().contents().contains("APPROVAL_BODY"));
            }
        }
        terminal.process(&captured.take());
        let visible = terminal.screen().contents();
        assert!(visible.contains("completed output"), "{visible}");
        assert!(
            !visible.contains("unsent draft") && !visible.contains("APPROVAL_BODY"),
            "{visible}"
        );
        assert_eq!(terminal.screen().cursor_position().1, 0);
    }
}

#[test]
fn leaving_preserves_shell_and_session_history_without_the_composer() {
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 300);
    terminal.process(b"shell history\r\noriginal prompt> ");
    let captured = CapturedOutput::default();
    let mut screen = screen();
    screen.out = Box::new(captured.clone());
    screen.interactive = true;
    screen.viewport.start_at(1, 17);
    set_input(&mut screen, "unsent draft");
    screen.set_footer(vec![Line::plain("directory"), Line::plain("stats")]);
    for index in 0..100 {
        screen.push_lines(vec![Line::plain(format!("session answer {index}"))]);
    }
    screen.flush();
    terminal.process(&captured.take());
    assert!(!terminal.screen().alternate_screen());
    assert!(terminal.screen().contents().contains("session answer 99"));
    screen.push_lines(vec![Line::plain("queued session output")]);
    // The resume hint is pushed the same way on the way out, so it lives or dies with this
    // path: if a queued line were dropped here, quitting would name no session to resume.
    screen.push_lines(vec![Line::plain("继续此会话：pi resume 01a1c13d")]);
    screen.leave();
    terminal.process(&captured.take());
    let visible = terminal.screen().contents();
    assert!(visible.contains("session answer 99") && visible.contains("queued session output"));
    assert!(
        visible.contains("继续此会话：pi resume 01a1c13d"),
        "the resume hint has to survive the exit: {visible}"
    );
    assert!(!visible.contains("draft") && !visible.contains("stats"));
    assert_eq!(terminal.screen().cursor_position().1, 0);
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    terminal.screen_mut().set_scrollback(300);
    let history = terminal.screen().contents();
    assert!(
        history.contains("shell history\noriginal prompt>"),
        "{history}"
    );
    assert!(history.contains("session answer 0"));
    screen.leave();
    assert!(
        captured.take().is_empty(),
        "leaving twice must not erase shell output"
    );
}

#[test]
fn streaming_blocks_commit_once_and_remain_in_native_scrollback() {
    let mut screen = screen();
    let captured = CapturedOutput::default();
    screen.out = Box::new(captured.clone());
    screen.interactive = true;
    set_input(&mut screen, "draft");
    screen.set_footer(vec![Line::plain("directory"), Line::plain("stats")]);
    screen.working = Some(WORKING_LABEL.into());
    screen.begin_stream();
    screen.push_thinking("thinking");
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 500);
    for index in 0..40 {
        screen.push_text(&format!("paragraph {index}\n\n"));
        screen.render();
        screen.flush();
        terminal.process(&captured.take());
        let expected_row = (2 + index * 2).min(20) as u16;
        assert_eq!(terminal.screen().cursor_position(), (expected_row, 7));
        assert!(
            terminal
                .screen()
                .contents()
                .ends_with("› draft\ndirectory\nstats")
        );
    }
    screen.push_text("unfinished tail");
    let (answer, thinking) = screen.end_stream();
    screen.flush();
    terminal.process(&captured.take());
    assert!(terminal.screen().contents().contains("unfinished tail"));
    assert!(answer.ends_with("unfinished tail"));
    assert_eq!(thinking, "thinking");
    let transcript = screen
        .blocks
        .iter()
        .flat_map(|block| block.render(40))
        .map(|line| line.text())
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        assert_eq!(
            transcript
                .lines()
                .filter(|line| *line == format!("paragraph {index}"))
                .count(),
            1
        );
    }
    assert!(!transcript.contains("思考完成"));
    assert!(!transcript.contains("thinking"));
    terminal.screen_mut().set_scrollback(500);
    let history = terminal.screen().contents();
    assert!(history.contains("paragraph 0"), "{history}");
    assert!(!history.contains("draft") && !history.contains("thinking"));
    terminal.screen_mut().set_scrollback(0);
    assert!(
        terminal
            .screen()
            .contents()
            .ends_with("› draft\ndirectory\nstats")
    );
    screen.absorb_event(Event::Paste(" more".into()));
    assert_eq!(input(&screen), "draft more");
}

#[test]
fn modal_views_preserve_native_history_without_duplicates_or_overlay_text() {
    fn terminal_rows(terminal: &mut vt100::Parser) -> Vec<String> {
        let width = terminal.screen().size().1.get();
        let mut rows = Vec::new();
        // Read every stored row once, then the visible screen. Checking only the oldest
        // page misses losses and duplicated history near the composer.
        for offset in (1..=terminal.screen().scrollback_len()).rev() {
            terminal.screen_mut().set_scrollback(offset);
            rows.push(terminal.screen().rows(0, width).next().unwrap());
        }
        terminal.screen_mut().set_scrollback(0);
        rows.extend(terminal.screen().rows(0, width));
        rows.retain(|row| !row.is_empty());
        rows
    }

    for history_count in [5, 60] {
        for view in ["/resume", "Ctrl+O", "approval"] {
            let captured = CapturedOutput::default();
            let mut terminal = vt100::Parser::new(nz(24), nz(80), 300);
            terminal.process(b"shell history\r\n$ pi\r\n");
            let mut screen = screen();
            screen.width = 80;
            screen.out = Box::new(captured.clone());
            screen.interactive = true;
            screen.viewport.start_at(2, 0);
            set_input(&mut screen, "unsent draft");
            screen.set_footer(vec![Line::plain("stats")]);
            let history: Vec<_> = (0..history_count)
                .map(|index| format!("history {index:03}"))
                .collect();
            screen.push_lines(history.iter().map(Line::plain).collect());
            screen.push(Block::collapsible(
                vec![Line::plain("tool summary"), Line::plain("detail only")],
                1,
                0,
                0,
            ));
            screen.flush();
            terminal.process(&captured.take());
            assert_eq!(terminal.screen().scrollback_len() > 0, history_count > 24);
            let mut expected = vec!["shell history".into(), "$ pi".into()];
            expected.extend(history);
            expected.push("tool summary".into());

            // Close normally once, then exit with the view still open. The latter must
            // restore an approval-covered tail even when no new output is pending.
            for exit in [false, true] {
                let mut decision = None;
                let title = match view {
                    "/resume" => {
                        screen.open_picker(
                            "恢复历史会话",
                            "Esc 取消",
                            (0..30)
                                .map(|n| Choice::new(format!("session {n}")))
                                .collect(),
                            0,
                        );
                        "恢复历史会话"
                    }
                    "Ctrl+O" => {
                        assert!(screen.show_tool_details());
                        "detail only"
                    }
                    "approval" => {
                        screen.suspend_live();
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        decision = Some(receiver);
                        screen.approval = Some((
                            crate::ui::auth_panel::PanelState::new(&"approval body\n".repeat(30)),
                            sender,
                        ));
                        "approval body"
                    }
                    _ => unreachable!(),
                };
                screen.flush();
                terminal.process(&captured.take());
                assert!(terminal.screen().contents().contains(title), "{view}");
                assert_eq!(terminal.screen().alternate_screen(), view == "Ctrl+O");
                if exit {
                    screen.leave();
                } else {
                    screen.push_lines(vec![Line::plain("output while view is open")]);
                    expected.push("output while view is open".into());
                    screen.flush();
                    screen.absorb_event(Event::Key(KeyCode::Esc.into()));
                    screen.flush();
                    if let Some(mut receiver) = decision {
                        assert_eq!(receiver.try_recv(), Ok(crate::auth::guard::Decision::Deny));
                    }
                }
                terminal.process(&captured.take());
                assert!(!terminal.screen().alternate_screen(), "{view}");
                let mut expected_rows = expected.clone();
                if !exit {
                    expected_rows.extend(["› unsent draft".into(), "stats".into()]);
                }
                assert_eq!(
                    terminal_rows(&mut terminal),
                    expected_rows,
                    "{view}, history_count={history_count}, exit={exit}"
                );
            }
        }
    }
}

#[test]
fn streamed_summaries_and_restored_markdown_have_the_same_layout() {
    let long_list = (1..=80)
        .map(|n| {
            format!("{n}. item {n}\n\n   continuation {n}\n\n   ```rust\n   call({n});\n   ```\n\n")
        })
        .collect::<String>();
    let fixtures = [
        "Intro\n\n1. first\n\n   continuation\n\n   - nested\n\n2. second\n\nDone",
        "Intro\n\n[说明][doc]\n\n**完成**\n\n[doc]: https://example.com\n",
        "Intro\n\n    let x = 1;\n\n| A | B |\n| - | - |\n| 中文 | **完成** |\n\nDone",
        &long_list,
    ];
    for source in fixtures {
        let mut screen = screen();
        screen.interactive = true;
        screen.out = Box::new(std::io::sink());
        screen.begin_stream();
        for chunk in source.chars().collect::<Vec<_>>().chunks(17) {
            screen.push_text(&chunk.iter().collect::<String>());
            let (live, _) = screen.compose_live();
            assert!(live.len() <= screen.height);
        }
        assert_eq!(screen.end_stream().0, source);
        for width in [12, 40, 80] {
            let streamed: Vec<HistoryRow<Line>> = screen
                .blocks
                .iter()
                .flat_map(|block| block.history_rows(width))
                .collect();
            let restored = Block::markdown(source).history_rows(width);
            assert_eq!(streamed, restored, "width {width}, {source:?}");
        }
    }
}

/// Everything the screen wrote, so a test can assert on the escape stream itself.
///
/// Shared with the agent's tests: they drive the same `Screen`, and a second copy of this
/// would be a second thing to keep in step.
#[derive(Clone, Default)]
pub(crate) struct CapturedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for CapturedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedOutput {
    pub(crate) fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    /// The same bytes as text, for assertions that only look for a phrase.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.take()).to_string()
    }
}

fn selected_code(terminal: &vt100::Screen) -> String {
    let (height, width) = terminal.size();
    let (height, width) = (height.get(), width.get());
    let rows = terminal.rows(0, width).collect::<Vec<_>>();
    let first = rows
        .iter()
        .position(|row| row.contains('┌'))
        .expect("code frame")
        + 1;
    let last = rows
        .iter()
        .position(|row| row.contains('└'))
        .expect("code frame end")
        - 1;
    assert!(last < usize::from(height));
    terminal.contents_between(first as u16, 0, last as u16, width)
}

#[test]
fn native_code_selection_keeps_source_newlines_and_indentation_while_streaming_and_after_resize() {
    // vt100 intentionally treats a wide character in the last column as a hard wrap.
    // Native Unicode output is checked separately without that emulator assumption.
    let body = "fn main() {\n    let text = \"words with    spaces that wrap\";\n    if ready {\n        emit(text);\n    }\n\n}";
    let mut screen = screen();
    let captured = CapturedOutput::default();
    screen.out = Box::new(captured.clone());
    screen.interactive = true;
    screen.height = 48;
    screen.width = 16;
    set_input(&mut screen, "draft");
    screen.set_footer(vec![Line::plain("stats")]);
    screen.working = Some(WORKING_LABEL.into());
    screen.begin_stream();
    screen.push_text(&format!("```rs\n{body}\n```"));
    let mut terminal = vt100::Parser::new(nz(48), nz(16), 500);
    for width in [16, 11, 32] {
        screen.width = width;
        terminal.screen_mut().set_size(nz(48), nz(width as u16));
        screen.render();
        screen.flush();
        terminal.process(&captured.take());
        assert_eq!(
            selected_code(terminal.screen()),
            body,
            "streaming, width {width}"
        );
        screen.tick_working();
        screen.flush();
        terminal.process(&captured.take());
        assert_eq!(
            selected_code(terminal.screen()),
            body,
            "spinner, width {width}"
        );
    }
    screen.end_stream();
    screen.flush();
    terminal.process(&captured.take());
    assert_eq!(selected_code(terminal.screen()), body, "completed");
    for width in [12, 23, 40, 80] {
        screen.width = width;
        terminal.screen_mut().set_size(nz(48), nz(width as u16));
        screen.render();
        screen.flush();
        terminal.process(&captured.take());
        assert_eq!(
            selected_code(terminal.screen()),
            body,
            "completed, width {width}"
        );
        assert!(!terminal.screen().alternate_screen());
        assert_eq!(
            terminal.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
    }
}

#[test]
fn native_selection_joins_visual_folds_but_keeps_explicit_prose_line_breaks() {
    let source = "first words with    spaces wrapping\nsecond original line";
    let mut screen = screen();
    let captured = CapturedOutput::default();
    screen.out = Box::new(captured.clone());
    screen.interactive = true;
    screen.height = 24;
    set_input(&mut screen, "draft");
    screen.push(Block::markdown(source));
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 100);
    for width in [40, 16, 11, 24] {
        screen.width = width;
        terminal.screen_mut().set_size(nz(24), nz(width as u16));
        screen.render();
        screen.flush();
        terminal.process(&captured.take());
        let rows = terminal.screen().rows(0, width as u16).collect::<Vec<_>>();
        let start = rows
            .iter()
            .position(|row| row.starts_with("first"))
            .unwrap();
        let end = rows
            .iter()
            .skip(start)
            .position(|row| row.is_empty())
            .unwrap()
            + start
            - 1;
        assert_eq!(
            terminal
                .screen()
                .contents_between(start as u16, 0, end as u16, width as u16),
            source,
            "width {width}: {rows:?}"
        );
    }
}

#[test]
fn native_code_selection_in_lists_and_quotes_keeps_only_the_code_indentation() {
    let body = "fn work() {\n    do_work();\n}";
    let listed = body
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let quoted = body
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    for source in [
        format!("- item\n\n  ```rs\n{listed}\n  ```"),
        format!("> ```rs\n{quoted}\n> ```"),
    ] {
        let mut screen = screen();
        screen.width = 12;
        screen.interactive = true;
        screen.out = Box::new(std::io::sink());
        screen.push(Block::markdown(source));
        let mut terminal = vt100::Parser::new(nz(24), nz(12), 100);
        terminal.process(screen.live_frame().as_bytes());
        assert_eq!(selected_code(terminal.screen()), body);
    }
}

#[test]
fn piped_output_keeps_source_lines_without_terminal_width_folds() {
    let captured = CapturedOutput::default();
    let mut screen = screen();
    screen.out = Box::new(captured.clone());
    screen.interactive = false;
    screen.width = 12;
    let source = "first words with    spaces and 中文 wrapping\n    original indentation";
    screen.push_lines(vec![Line::plain(source)]);
    screen.flush();
    assert_eq!(
        String::from_utf8(captured.take()).unwrap(),
        format!("{source}\n")
    );
}

#[test]
fn native_unicode_history_has_no_inserted_breaks_spaces_or_background_padding() {
    for width in [11, 16, 23] {
        let source = format!("{}中文 👩‍💻 continued with    spaces", "x".repeat(width - 1));
        let mut screen = screen();
        screen.width = width;
        screen.interactive = true;
        screen.out = Box::new(std::io::sink());
        screen.push_lines(vec![Line::new(
            &source,
            Style::with_bg(Color::DiffAddedText, Bg::Added),
        )]);
        let output = util::strip_ansi(&screen.live_frame());
        assert!(output.contains(&source), "width {width}: {output:?}");
    }
}

#[test]
fn large_pastes_queues_images_and_menus_fit_even_small_terminals() {
    for height in [1, 2, 3, 6, 12, 24] {
        for width in [1, 2, 8, 20, 40] {
            let mut screen = screen_with_commands();
            screen.width = width;
            screen.height = height;
            set_input(&mut screen, &"一行中文 👩‍💻\n".repeat(100));
            screen.pending = (0..100)
                .map(|index| Queued::new(format!("queued {index}"), Vec::new()))
                .collect();
            screen.pending_images = (0..10)
                .map(|_| PastedImage {
                    width: 1,
                    height: 1,
                    data: "AA==".into(),
                    media_type: "image/png",
                    bytes: 3,
                })
                .collect();
            screen.streaming_answer = Some("output\n".repeat(100));
            screen.working = Some(WORKING_LABEL.into());
            screen.set_footer(vec![Line::plain("directory"), Line::plain("stats")]);
            let (lines, cursor) = screen.compose_live();
            assert!(
                lines.len() <= height,
                "{width}×{height}: {} rows",
                lines.len()
            );
            assert!(lines.iter().all(|line| line.line.width() <= width));
            let (row, column) = cursor.unwrap();
            assert!(row < lines.len() && column < width);
            set_input(&mut screen, "/");
            screen.sync_menu();
            screen.menu_selected = screen.menu.len() - 1;
            assert!(screen.compose_live().0.len() <= height);
        }
    }
}

#[test]
fn newline_keys_edit_and_vertical_arrows_do_not_replace_a_multiline_draft() {
    let mut screen = screen();
    screen.history = vec!["previous".into()];
    set_input(&mut screen, "first");
    for key in [
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ] {
        assert!(screen.handle_key(key).is_none());
    }
    assert_eq!(input(&screen), "first\n\n\n");
    screen.handle_key(KeyEvent::from(KeyCode::Up));
    assert_eq!(input(&screen), "first\n\n\n");
    assert!(screen.history_index.is_none());
    for _ in 0..3 {
        screen.handle_key(KeyEvent::from(KeyCode::Up));
    }
    assert_eq!(input(&screen), "previous");
    screen.handle_key(KeyEvent::from(KeyCode::Down));
    assert_eq!(input(&screen), "first\n\n\n");
}

#[test]
fn large_pickers_keep_every_selected_item_visible() {
    let items = (0..100)
        .map(|index| format!("model {index}"))
        .collect::<Vec<_>>();
    for height in [1, 2, 3, 8, 24] {
        let mut screen = screen();
        screen.height = height;
        for selected in [0, 1, 50, 99] {
            let choices: Vec<Choice> = items.iter().map(Choice::new).collect();
            let rows = screen.choice_lines("模型", "Enter 确认", &choices, selected);
            assert!(rows.len() <= height);
            assert!(
                rows.iter()
                    .any(|line| line.text().trim() == format!("› model {selected}")),
                "{height}: {selected}"
            );
        }
    }
}

#[test]
fn a_described_choice_puts_its_detail_in_the_column_beside_it() {
    // What `/permissions` draws: the label on the left, the explanation in its own column on
    // the same row. Reading down that column is how the two answers are compared; putting
    // the detail on a second row instead would make the reader hold one entry in their head
    // while reading the other.
    use crate::auth::guard::PermissionMode;
    let choices = vec![
        Choice::with_detail(PermissionMode::Ask.label(), PermissionMode::Ask.detail()),
        Choice::with_detail(
            PermissionMode::Allow.label(),
            PermissionMode::Allow.detail(),
        ),
    ];
    let mut screen = screen();
    screen.height = 24;
    let rows = screen.choice_lines("命令审核", "Enter 确认", &choices, 0);

    assert_eq!(rows[0].text(), "命令审核");
    // Two entries, two rows: no entry is ever two rows tall.
    assert_eq!(
        rows.len(),
        4,
        "{:?}",
        rows.iter().map(|r| r.text()).collect::<Vec<_>>()
    );

    let first = rows[1].text();
    assert!(first.starts_with("› 需要审核"), "{first:?}");
    assert!(first.contains(PermissionMode::Ask.detail()), "{first:?}");
    let second = rows[2].text();
    assert!(second.starts_with("  自动放行"), "{second:?}");
    assert!(
        second.contains(PermissionMode::Allow.detail()),
        "{second:?}"
    );

    // Both details start at the same column — past the widest label — so the two read as one
    // aligned column rather than each hugging its own label. The column is measured on the
    // spans, not by searching the padded text: the padding is spaces, and `find` on a string
    // that has already been padded would count them.
    let detail_at = |row: &Line| {
        let label = util::width(&row.spans[0].text);
        assert!(row.spans.len() > 1, "no detail span: {:?}", row.text());
        label
    };
    assert_eq!(
        detail_at(&rows[1]),
        detail_at(&rows[2]),
        "{first:?} / {second:?}"
    );
    assert!(detail_at(&rows[1]) > util::width("› 需要审核"));
    // The label column is the widest label plus the gap, so both entries share one start.
    assert_eq!(
        detail_at(&rows[1]),
        2 + util::width("需要审核") + 2,
        "{first:?}"
    );

    // Each detail says only its own half: both entries run the same policy, so the shared
    // part belongs in neither column.
    assert_ne!(PermissionMode::Ask.detail(), PermissionMode::Allow.detail());
    assert!(!PermissionMode::Ask.detail().contains("需要确认"));
    assert!(!PermissionMode::Allow.detail().contains("需要确认"));

    // The selection paints the whole entry — label, detail and the rest of the row — so a
    // selected line is one bar rather than two painted islands.
    assert_eq!(util::width(&first), screen.width);
    assert!(
        rows[1]
            .spans
            .iter()
            .all(|span| span.style.bg == Bg::Selected)
    );
    assert!(
        rows[2]
            .spans
            .iter()
            .all(|span| span.style.bg != Bg::Selected)
    );
}

#[test]
fn every_described_choice_fits_the_shortest_terminal() {
    // A detail column is optional, so the row count never depends on it: whatever the
    // height, the entries that fit are drawn and the rest scroll. This is the invariant that
    // a per-entry second row used to break at height 1.
    let choices: Vec<Choice> = (0..6)
        .map(|index| Choice::with_detail(format!("选项 {index}"), format!("说明 {index}")))
        .collect();
    for height in [1, 2, 3, 4, 8, 24] {
        let mut screen = screen();
        screen.height = height;
        for selected in 0..choices.len() {
            let rows = screen.choice_lines("标题", "Enter 确认", &choices, selected);
            assert!(
                rows.len() <= height,
                "{height}: {selected} -> {}",
                rows.len()
            );
            assert!(
                rows.iter().any(|line| line
                    .text()
                    .trim_start_matches(['›', '↑', '↓', ' '])
                    .starts_with(&format!("选项 {selected}"))),
                "{height}: {selected} -> {:?}",
                rows.iter().map(|r| r.text()).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn a_detail_column_that_does_not_fit_is_dropped_not_wrapped() {
    // On a narrow terminal the detail is left out. Wrapping it onto a second row would give
    // back the layout this replaced, and truncating it to a stub would print a few characters
    // the reader cannot connect to anything.
    let choices = vec![
        Choice::with_detail("需要审核", "执行前先问你一句"),
        Choice::with_detail("自动放行", "直接执行，不再询问"),
    ];
    let mut screen = screen();
    screen.height = 24;
    // Wide enough for the labels, short of the detail column.
    screen.width = util::width("  自动放行") + 2;
    let rows = screen.choice_lines("标题", "Enter 确认", &choices, 0);
    // The rows are the title, the two entries in order, and the hint — every one of them
    // present, because dropping the column must not cost anything else.
    let drawn: Vec<String> = rows.iter().map(|row| row.text()).collect();
    assert_eq!(drawn.len(), 4, "{drawn:?}");
    assert_eq!(drawn[0], "标题");
    assert!(drawn[1].starts_with("› 需要审核"), "{drawn:?}");
    assert!(drawn[2].starts_with("  自动放行"), "{drawn:?}");
    assert!(drawn[3].contains("Enter 确认"), "{drawn:?}");
    // No entry grew a second row and no detail survived in any form.
    for row in &rows {
        let text = row.text();
        assert!(!text.contains('…'), "{text:?}");
        assert!(!text.contains("执行前"), "{text:?}");
    }
    // Entry rows are still painted to the full width, so the selection stays a solid bar.
    for row in &rows[1..3] {
        assert_eq!(util::width(&row.text()), screen.width, "{:?}", row.text());
    }
    // The labels survive: dropping the column must not cost the reader the choice itself.
    assert!(
        rows[1].text().starts_with("› 需要审核"),
        "{:?}",
        rows[1].text()
    );
    assert!(
        rows[2].text().starts_with("  自动放行"),
        "{:?}",
        rows[2].text()
    );
}

#[test]
fn a_fitting_list_is_not_numbered() {
    // The counter is for a list that does not fit. With two entries both on screen, "1/2"
    // is a number the user has to read and then dismiss.
    let choices = vec![Choice::new("甲"), Choice::new("乙")];
    let mut screen = screen();
    screen.height = 24;
    let rows = screen.choice_lines("命令审核", "Enter 确认", &choices, 0);
    assert_eq!(rows[0].text(), "命令审核");

    // Past the window it comes back, because there it is answering "how much is there".
    let many: Vec<Choice> = (0..40)
        .map(|index| Choice::new(format!("项 {index}")))
        .collect();
    screen.height = 8;
    let rows = screen.choice_lines("模型", "Enter 确认", &many, 3);
    assert!(
        rows[0].text().starts_with("模型  4/40"),
        "{}",
        rows[0].text()
    );
}

#[test]
fn resizing_and_modal_overlays_restore_history_at_the_current_width() {
    let mut screen = screen();
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 100);
    set_input(&mut screen, "draft");
    screen.set_footer(vec![Line::plain("stats")]);
    screen.push_lines(vec![
        Line::plain("short history"),
        Line::plain("a long history line that must wrap when narrowed"),
    ]);
    terminal.process(screen.live_frame().as_bytes());
    terminal.process(screen.viewport.erase().as_bytes());
    terminal.process(b"\x1b[H\x1b[2Joverlay");
    terminal.process(screen.live_frame().as_bytes());
    assert!(terminal.screen().contents().contains("short history"));
    assert!(!terminal.screen().contents().contains("overlay"));
    screen.width = 16;
    screen.height = 10;
    terminal.screen_mut().set_size(nz(10), nz(16));
    terminal.process(screen.live_frame().as_bytes());
    assert_eq!(terminal.screen().cursor_position(), (4, 7));
    assert!(
        terminal.screen().contents().contains("narrowed"),
        "{}",
        terminal.screen().contents()
    );
    assert!(terminal.screen().contents().ends_with("› draft\nstats"));
}

#[test]
fn the_status_reflows_without_new_model_output_after_a_resize() {
    let mut screen = screen();
    set_input(&mut screen, "");
    let model = crate::config::ModelConfig {
        id: "example-model".into(),
        ..Default::default()
    };
    screen.set_footer_state(FooterState {
        cwd: "/tmp/project".into(),
        branch: None,
        session_name: None,
        totals: crate::config::Usage::default(),
        cache_hit_rate: None,
        output_rate: None,
        context_usage: crate::ui::footer::ContextUsage::Tokens(500),
        context_window: Some(100_000),
        model: Some(model),
        level: String::new(),
        busy: None,
    });
    screen.width = 80;
    let wide = screen.compose_live().0.last().unwrap().line.text();
    screen.width = 20;
    let narrow = screen.compose_live().0.last().unwrap().line.text();
    assert!(wide.contains("example-model"));
    assert!(narrow.contains("example"), "{narrow}");
    assert!(util::width(&narrow) <= 20);
}

#[test]
fn pasted_paragraphs_wait_for_enter_at_the_prompt_and_during_a_turn() {
    for busy in [false, true] {
        let mut screen = screen();
        screen.begin_line();
        if busy {
            screen.set_working("工作中");
        }
        let text = "第一段\n\n第二段\n/exit\n";
        assert!(screen.absorb_event(Event::Paste(text.into())).is_none());
        assert_eq!(input(&screen), text);
        assert!(screen.history.is_empty());
        assert!(screen.pending.is_empty());
        let action = screen.absorb_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(matches!(action, Some(Action::Line(line)) if line == text));
        assert_eq!(screen.history, vec![text.to_string()]);
        assert_eq!(input(&screen), "");
    }
}

#[test]
fn pasting_newlines_does_not_accept_a_command_menu_entry() {
    let mut screen = screen();
    screen.begin_line();
    set_input(&mut screen, "/");
    screen.sync_menu();
    assert!(
        screen
            .absorb_event(Event::Paste("model\n\n".into()))
            .is_none()
    );
    assert!(screen.menu.is_empty());
    assert!(matches!(screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter, KeyModifiers::NONE,
    ))), Some(Action::Line(line)) if line == "/model\n\n"));
}

/// vt100 0.18 takes `NonZeroU16` for its dimensions: passing 0 panicked before, so the
/// type now says so. The test dimensions are literals and never zero.
fn nz(value: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(value).expect("test dimensions are never zero")
}

fn screen() -> Screen {
    let mut screen = Screen::new();
    screen.out = Box::new(Vec::<u8>::new());
    screen.interactive = false;
    screen.width = 40;
    screen.height = 24;
    screen
}

/// Set the input buffer the way a test means it: text, with the caret at the end.
fn set_input(screen: &mut Screen, text: &str) {
    screen.editing = Some(Editor::from_text(text));
}

/// The input buffer as a string.
fn input(screen: &Screen) -> String {
    screen
        .editing
        .as_ref()
        .map(Editor::text)
        .unwrap_or_default()
}

/// Put the caret `back` characters from the end of the buffer.
fn caret_back(screen: &mut Screen, back: usize) {
    if let Some(editor) = &mut screen.editing {
        editor.home();
        editor.move_caret((editor.len() - back.min(editor.len())) as isize);
    }
}

#[test]
fn piped_output_is_committed_on_exit_without_terminal_controls() {
    let captured = CapturedOutput::default();
    let mut screen = screen();
    screen.out = Box::new(captured.clone());
    screen.push_lines(vec![Line::plain("last answer")]);
    screen.leave();
    assert_eq!(captured.take(), b"last answer\n");
    screen.leave();
    assert!(captured.take().is_empty());
}

#[test]
fn the_window_title_names_the_project_not_its_path() {
    // The tab shows which project this is; where it lives is not the question a tab
    // answers, and the path is what made the title unreadable.
    let cwd = Path::new("/home/me/文档/mpi");
    assert_eq!(window_title(None, cwd), "π - mpi");
    // A session name wins: the user chose it deliberately.
    assert_eq!(window_title(Some("重构解析器"), cwd), "π - 重构解析器");
    assert_eq!(window_title(Some("   "), cwd), "π - mpi");
    // Only the last component is used, so a deeply nested checkout stays short.
    assert_eq!(
        window_title(None, Path::new("/a/b/c/deep-project")),
        "π - deep-project"
    );
    // The file-system root has no name to show.
    assert_eq!(window_title(None, Path::new("/")), "π");
}

#[test]
fn a_session_name_cannot_break_out_of_the_title_sequence() {
    // The name is user input. A raw ESC or BEL here would end the OSC sequence early and
    // let whatever follows be read as a terminal command, so both are stripped.
    let cwd = Path::new("/tmp/work");
    let title = window_title(Some("a\u{1b}]0;evil\u{7}b"), cwd);
    assert!(!title.contains('\u{1b}'), "{title:?}");
    assert!(!title.contains('\u{7}'), "{title:?}");
    // The injected sequence is removed outright, not merely neutralised.
    assert_eq!(title, "π - ab");
    // A bare BEL is dropped as well.
    assert_eq!(window_title(Some("a\u{7}b"), cwd), "π - ab");
    // A newline would split the title across two lines in the terminal's tab bar.
    assert!(!window_title(Some("a\nb"), cwd).contains('\n'));
}

#[test]
fn a_piped_screen_does_not_write_a_title() {
    // `screen()` is non-interactive, so nothing should reach stdout.
    let mut screen = screen();
    screen.set_title(Some("demo"), Path::new("/tmp/work"));
    assert!(screen.title.is_none());
}

fn screen_with_commands() -> Screen {
    let mut screen = screen();
    screen.set_commands(crate::agent::r#loop::COMMANDS);
    screen
}

#[test]
fn the_working_spinner_sits_above_the_input_and_moves() {
    // pi's indicator: a frame that advances, at the left of the input area. Its whole
    // job is to be *moving* — a still mark cannot be told apart from a hung process, so
    // the frames are asserted to differ rather than merely to exist.
    let mut screen = screen_with_commands();
    set_input(&mut screen, "");
    screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
    screen.working = Some(WORKING_LABEL.to_string());

    let (lines, _) = screen.compose_live();
    let input = lines
        .iter()
        .position(|line| line.line.text().starts_with('›'))
        .unwrap();
    let text: Vec<String> = lines.iter().map(|row| row.line.text()).collect();
    assert_eq!(input, 1, "the spinner is one row: {text:?}");
    assert_eq!(
        lines[0].line.text(),
        format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL)
    );
    let label = lines[0]
        .line
        .spans
        .iter()
        .find(|span| span.text == WORKING_LABEL)
        .unwrap();
    assert_eq!(
        label.style.fg,
        Color::Blue,
        "Working is blue, not the hint grey"
    );

    // Each tick advances by exactly one frame and wraps, so the animation has no jump.
    screen.working_frame = WORKING_FRAMES.len() - 1;
    screen.tick_working();
    let (lines, _) = screen.compose_live();
    assert_eq!(
        lines[0].line.text(),
        format!("{} {}", WORKING_FRAMES[0], WORKING_LABEL)
    );
    screen.tick_working();
    let (lines, _) = screen.compose_live();
    assert_eq!(
        lines[0].line.text(),
        format!("{} {}", WORKING_FRAMES[1], WORKING_LABEL)
    );
}

#[test]
fn the_spinner_is_gone_once_the_turn_ends() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "");
    screen.working = Some(WORKING_LABEL.to_string());
    screen.clear_working();
    let (lines, _) = screen.compose_live();
    let text: Vec<String> = lines.iter().map(|row| row.line.text()).collect();
    assert!(
        lines[0].line.text().starts_with('›'),
        "the input is the first row again: {text:?}"
    );
}

#[test]
fn a_slash_opens_the_menu_and_filters_as_more_is_typed() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/");
    screen.sync_menu();
    assert_eq!(screen.menu.len(), crate::agent::r#loop::COMMANDS.len());
    // The first entry is highlighted, so Enter has an unambiguous target.
    assert_eq!(screen.menu[0].0, "model");

    set_input(&mut screen, "/m");
    screen.sync_menu();
    // "m" matches /model and /compact: the match is on the name, not the description.
    assert_eq!(screen.menu.len(), 1, "{:?}", screen.menu);
    assert_eq!(screen.menu[0].0, "model");

    set_input(&mut screen, "/na");
    screen.sync_menu();
    assert_eq!(screen.menu.len(), 1);
    assert_eq!(screen.menu[0].0, "name");
}

#[test]
fn the_menu_stays_out_of_the_way_of_ordinary_text() {
    let mut screen = screen_with_commands();
    // A slash mid-sentence is not a command, and an argument is being typed once there
    // is a space, so in both cases there is nothing to complete.
    for text in ["你好/世界", "/name 我的会话", "no slash at all"] {
        set_input(&mut screen, text);
        screen.sync_menu();
        assert!(screen.menu.is_empty(), "{text:?} opened a menu");
    }
}

#[test]
fn tab_completes_a_unique_command_along_with_a_space() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/na");
    assert!(screen.complete());
    // The trailing space means the argument can be typed straight away.
    assert_eq!(input(&screen), "/name ");
    // The menu closes because the name is settled.
    assert!(screen.menu.is_empty());
}

#[test]
fn tab_shares_a_prefix_before_cycling_through_the_menu() {
    let mut screen = screen_with_commands();
    // "co" matches only /compact, so that case is covered above; "c" also matches
    // nothing else, so use two commands sharing a prefix via the real list.
    set_input(&mut screen, "/");
    assert!(screen.complete() || !screen.menu.is_empty());
    // With several matches and no shared prefix to add, Tab walks the highlight.
    let before = screen.menu_selected;
    screen.complete();
    assert_ne!(screen.menu_selected, before);
}

#[test]
fn tab_on_an_exact_command_does_nothing_destructive() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/exit");
    screen.sync_menu();
    // /exit is the only match, so Tab would fill in the space; the point is that it
    // must not lose what was typed.
    screen.complete();
    assert!(input(&screen).starts_with("/exit"));
}

#[test]
fn a_shared_prefix_is_filled_in_before_cycling() {
    assert_eq!(common_prefix(&["model", "modify"]), "mod");
    assert_eq!(common_prefix(&["name", "new"]), "n");
    // No shared prefix: the empty string, which is always a prefix, and the caller
    // then falls through to cycling the menu instead of typing anything.
    assert_eq!(common_prefix(&["model", "exit"]), "");
    assert_eq!(common_prefix(&["only"]), "only");
    assert_eq!(common_prefix(&[]), "");
}

#[test]
fn a_menu_selection_is_the_command_that_runs() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/re");
    screen.sync_menu();
    // Enter takes the highlighted entry as a whole command: the menu is there to save
    // typing, so picking from it must not require a second Enter to submit.
    assert_eq!(screen.accepted_command().as_deref(), Some("/resume"));
    assert_eq!(screen.menu.len(), 1, "the filter left only the match");

    // Moving the highlight moves what Enter would run.
    set_input(&mut screen, "/");
    screen.sync_menu();
    screen.move_menu(1);
    assert_eq!(screen.accepted_command(), Some("/name".into()));

    // With no menu open there is nothing to accept, and the buffer is submitted as
    // typed.
    set_input(&mut screen, "你好");
    screen.sync_menu();
    assert_eq!(screen.accepted_command(), None);
}

#[test]
fn the_menu_wraps_at_both_ends() {
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/");
    screen.sync_menu();
    assert!(screen.move_menu(-1));
    assert_eq!(screen.menu_selected, screen.menu.len() - 1);
    assert!(screen.move_menu(1));
    assert_eq!(screen.menu_selected, 0);
}

#[test]
fn the_cursor_lands_after_the_buffer_not_below_the_footer() {
    // The live region is drawn as text; nothing moves the cursor unless the drawing
    // code says where it goes. Without this the caret ends up under the footer.
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/mo");
    screen.sync_menu();
    screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
    let (lines, cursor) = screen.compose_live();
    let (row, column) = cursor.expect("an editing screen has a cursor");
    // The prompt is two columns, and the buffer is three characters.
    assert_eq!(column, 2 + 3);
    // The row holds the input line — not the footer, which is below it.
    assert!(
        lines[row].line.text().starts_with("› /mo"),
        "{:?}",
        lines[row].line.text()
    );
    assert!(row + 1 < lines.len());
    assert!(
        lines[row + 1].line.text().contains("/model"),
        "the menu goes under the input"
    );
    // Opening, filtering and closing the menu must restore the history it covered.
    for history_len in [0, 2, 40] {
        let mut screen = screen_with_commands();
        screen.set_footer(vec![Line::plain("dir"), Line::plain("stats")]);
        screen.push(Block::lines(
            (0..history_len)
                .map(|index| Line::plain(format!("history {index}")))
                .collect(),
        ));
        let mut terminal = vt100::Parser::new(nz(24), nz(40), 0);
        for input in ["", "/", "/mo", "/", ""] {
            set_input(&mut screen, input);
            screen.sync_menu();
            terminal.process(screen.live_frame().as_bytes());
            let visible = terminal.screen().contents();
            let (row, _) = terminal.screen().cursor_position();
            if history_len > 0 {
                assert_eq!(
                    terminal
                        .screen()
                        .rows(0, 40)
                        .nth(usize::from(row) - 1)
                        .unwrap(),
                    format!("history {}", history_len - 1),
                    "menu {input:?} left a gap above the composer: {visible}"
                );
            } else {
                assert_eq!(row, 0, "empty session moved the composer: {visible}");
            }
        }
    }
}

#[test]
fn a_recalled_command_does_not_open_the_menu_over_the_arrows() {
    // The two requirements pull in opposite directions and this is where they meet: the
    // menu must be selectable with the arrows, and Down at the end of the history must
    // reach a blank line. They conflicted because recalling a `/command` popped the menu
    // open on it, and the arrows then belonged to a list nobody asked for. A recall is not
    // typing, so it does not open the menu — which leaves the arrows free for the history.
    let mut screen = screen_with_commands();
    screen.history = vec!["/name".into(), "hello".into()];
    set_input(&mut screen, "");
    screen.history_index = Some(0);

    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
    assert_eq!(input(&screen), "/name", "Up recalls the previous line");
    assert!(
        screen.menu.is_empty(),
        "recalling a command must not open the menu"
    );

    // Down is the way back out of the history, and it still works.
    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
    assert_eq!(
        input(&screen),
        "hello",
        "Down leaves the recalled command behind"
    );
    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
    assert_eq!(
        input(&screen),
        "",
        "and the last Down reaches the blank line"
    );
}

#[test]
fn the_arrows_select_from_the_menu_while_it_is_open() {
    // Typing `/` opens the menu, and the arrows have to move through it: that is how a
    // menu is used everywhere else, and the highlight is the only thing that says which
    // entry Enter would run.
    let mut screen = screen_with_commands();
    screen.begin_line();
    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Char('/'))));
    assert!(screen.menu.len() > 1, "the menu lists the commands");
    assert_eq!(screen.menu_selected, 0);

    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Down)));
    assert_eq!(screen.menu_selected, 1, "Down moves the highlight");
    assert_eq!(input(&screen), "/", "and leaves the buffer alone");

    screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Up)));
    assert_eq!(screen.menu_selected, 0, "Up moves it back");
}

#[test]
fn esc_stops_the_turn_only_when_something_is_running() {
    // Esc has two jobs and they must not collide: it closes an open menu, and it stops a
    // turn. With nothing running and no menu it stays a no-op — it cannot be a Stop that
    // nothing is listening for, because `read_input` at the prompt would then have to
    // filter it back out.
    let mut screen = screen_with_commands();
    set_input(&mut screen, "hello");
    screen.sync_menu();
    assert!(screen.menu.is_empty());

    // Nothing running: Esc is swallowed.
    assert!(
        screen
            .absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .is_none(),
        "Esc at an idle prompt is not an action"
    );

    // A turn in flight: Esc is the stop.
    screen.working = Some(WORKING_LABEL.to_string());
    assert_eq!(
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
        Some(Action::Stop)
    );
}

#[test]
fn esc_closes_an_open_menu_instead_of_stopping_the_turn() {
    // The menu wins: it is a list the user is being asked about, and dismissing it must
    // not cost them the answer being written behind it. The turn only stops on the Esc
    // *after* the menu is gone, which is what makes the two jobs separable by pressing
    // the key twice.
    let mut screen = screen_with_commands();
    screen.working = Some(WORKING_LABEL.to_string());
    set_input(&mut screen, "/mo");
    screen.sync_menu();
    assert!(
        !screen.menu.is_empty(),
        "typing a command name opens the menu"
    );

    assert!(
        screen
            .absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc)))
            .is_none(),
        "the first Esc only closes the menu"
    );
    assert!(screen.menu.is_empty());
    assert_eq!(input(&screen), "/mo", "and the buffer survives");

    assert_eq!(
        screen.absorb_event(Event::Key(KeyEvent::from(KeyCode::Esc))),
        Some(Action::Stop),
        "the second Esc, with no menu left, stops the turn"
    );
}

#[test]
fn esc_closes_the_menu_and_it_stays_closed() {
    // Esc had no effect at all before: the redraw at the end of the same keypress put the
    // list straight back. It is the buffer the user dismissed, so typing on brings the
    // menu back — a dismissal that survived to the end of the line would make it feel
    // broken in the other direction.
    let mut screen = screen_with_commands();
    set_input(&mut screen, "/mo");
    screen.sync_menu();
    assert!(!screen.menu.is_empty());

    // What the Esc key does.
    screen.menu_dismissed = screen.editing.as_ref().map(Editor::text);
    screen.menu.clear();
    screen.sync_menu();
    assert!(screen.menu.is_empty(), "the redraw must not bring it back");

    // The buffer moves on, so the dismissal no longer describes what is on screen.
    set_input(&mut screen, "/mod");
    screen.sync_menu();
    assert!(
        !screen.menu.is_empty(),
        "a new command name gets its menu back"
    );
    assert!(screen.menu_dismissed.is_none());
}

#[test]
fn slash_commands_dispatch_immediately_without_blocking_ordinary_messages() {
    let mut screen = screen_with_commands();
    for text in ["  /model  ", "/compact", "/name x", "/exit"] {
        let command = crate::agent::r#loop::queue_mid_turn(&mut screen, text.into(), Vec::new());
        assert_eq!(command.as_deref(), Some(text.trim()));
        assert!(!screen.has_steering());
    }
    for text in ["  first  ", "second", "   "] {
        assert!(
            crate::agent::r#loop::queue_mid_turn(&mut screen, text.into(), Vec::new()).is_none()
        );
    }
    assert_eq!(
        screen.take_steering(),
        [
            Queued::new("first".into(), Vec::new()),
            Queued::new("second".into(), Vec::new())
        ]
    );
    assert!(screen.pop_queued().is_none());
}

#[test]
fn a_paste_that_was_never_submitted_stays_with_the_draft() {
    // `pop_queued` used to sweep `pending_images` into the queue, which sent a
    // screenshot the user was still composing and left the message it belonged to
    // without it. Images travel with the line on Enter, so the queue never owns them.
    let mut screen = screen_with_commands();
    screen.pending_images.push(crate::image_input::PastedImage {
        width: 4,
        height: 4,
        data: "AAAA".into(),
        media_type: "image/png",
        bytes: 3,
    });
    crate::agent::r#loop::queue_mid_turn(&mut screen, "queued message".to_string(), Vec::new());

    let queued = drain_queued(&mut screen);
    assert_eq!(queued.len(), 1);
    assert_eq!(
        screen.pending_images.len(),
        1,
        "the unsubmitted paste still belongs to the line being written"
    );
    assert_eq!(queued[0].text, "queued message");
    assert!(queued[0].images.is_empty());
}

#[test]
fn arming_the_next_line_keeps_the_draft() {
    // The composer is armed again after every turn, and the user may have started
    // writing during the turn that just ended. `begin_line` used to blank the buffer,
    // which threw that message away one turn later than the keystroke bug it fixed.
    let mut screen = screen_with_commands();
    set_input(&mut screen, "half-written");
    screen.begin_line();
    assert_eq!(input(&screen), "half-written");
}

#[test]
fn submitting_a_line_ends_the_history_walk() {
    // Enter takes the recalled line and empties the buffer. The walk has to end there
    // too: an index still pointing into the history makes the next Down continue the
    // walk from a line that is no longer on screen.
    let mut screen = screen_with_commands();
    screen.history = vec!["first".into(), "second".into()];
    set_input(&mut screen, "");
    screen.history_up();
    assert_eq!(input(&screen), "second");
    assert!(screen.history_index.is_some());

    screen.handle_key(KeyEvent::from(KeyCode::Enter));

    assert_eq!(input(&screen), "", "the line is on its way out");
    assert!(screen.history_index.is_none(), "the walk is over");
    assert!(screen.history_draft.is_none());
}

#[test]
fn walking_back_down_past_the_newest_entry_restores_the_draft() {
    // Up is a look at the history, not a commitment: a half-written message must survive
    // the glance. Without the stash, the last Down would blank the line and the user's
    // text would be gone with no sign that it had ever been there.
    let mut screen = screen_with_commands();
    screen.history = vec!["first".into(), "second".into()];
    set_input(&mut screen, "my draft");

    screen.history_up();
    assert_eq!(input(&screen), "second");
    screen.history_up();
    assert_eq!(input(&screen), "first");
    screen.history_down();
    assert_eq!(input(&screen), "second");
    screen.history_down();
    assert_eq!(
        input(&screen),
        "my draft",
        "the draft comes back, not a blank line"
    );
    assert!(screen.history_index.is_none(), "the walk is over");
}

#[test]
fn down_without_a_walk_in_progress_does_nothing() {
    // Down belongs to the menu-less prompt as the "blank line" key only while walking;
    // on a fresh buffer it must not clear what is being typed.
    let mut screen = screen_with_commands();
    screen.history = vec!["first".into()];
    set_input(&mut screen, "typing away");
    screen.history_down();
    assert_eq!(input(&screen), "typing away");
}

#[test]
fn up_at_the_oldest_entry_stays_there() {
    // No wrap-around: with one, a repeated key press silently changes which entry is on
    // screen and the top of the history is indistinguishable from the bottom.
    let mut screen = screen_with_commands();
    screen.history = vec!["only".into()];
    screen.history_up();
    screen.history_up();
    assert_eq!(input(&screen), "only");
    assert_eq!(screen.history_index, Some(0));
}

#[test]
fn a_resumed_session_offers_the_lines_it_already_holds() {
    // The bug this guards: history lived only in the process, so Up after a resume
    // reached nothing that was typed before it — the turns in the file looked like they
    // had never been typed, and the user had to retype a line that was already there.
    let mut screen = screen_with_commands();
    screen.seed_history(["asked before resuming".to_string(), "and again".to_string()]);

    screen.history_up();
    assert_eq!(
        input(&screen),
        "and again",
        "the newest seeded line comes first"
    );
    screen.history_up();
    assert_eq!(input(&screen), "asked before resuming");
    screen.history_up();
    assert_eq!(
        input(&screen),
        "asked before resuming",
        "the top of the history"
    );

    // A line typed after the resume joins the same list, in the order it was typed.
    set_input(&mut screen, "typed just now");
    screen.remember("typed just now");
    screen.history_down();
    screen.history_down();
    assert_eq!(input(&screen), "typed just now");
}

#[test]
fn seeding_replaces_the_list_it_inherits() {
    // Seeding happens when the screen changes which conversation it is showing — a
    // resume, a `/resume` to another session, a `/new`. The lines that came with the
    // session being left are not history for the one being opened, and keeping them
    // would put a message from one conversation into the arrows of another.
    let mut screen = screen_with_commands();
    screen.history = vec!["from the last session".into()];
    screen.history_index = Some(0);
    screen.history_draft = Some(Editor::from_text("half written"));

    screen.seed_history(["from this one".to_string()]);

    assert_eq!(screen.history, vec!["from this one".to_string()]);
    assert!(
        screen.history_index.is_none(),
        "the walk belongs to the old list"
    );
    assert!(screen.history_draft.is_none());

    // A new session has nothing to recall.
    screen.seed_history(Vec::new());
    assert!(screen.history.is_empty());
}

#[test]
fn seeding_keeps_the_newest_entries() {
    // A long session has more lines than the cap; what is kept has to be the tail, since
    // that is what a recalled line is likely to be about.
    let mut screen = screen_with_commands();
    screen.seed_history((0..HISTORY_LIMIT + 5).map(|i| format!("line {i}")));

    assert_eq!(screen.history.len(), HISTORY_LIMIT);
    assert_eq!(
        screen.history[0], "line 5",
        "the oldest entries are the ones dropped"
    );
    assert_eq!(
        screen.history[HISTORY_LIMIT - 1],
        format!("line {}", HISTORY_LIMIT + 4)
    );
}

#[test]
fn blank_seeded_lines_are_not_entries() {
    // Up on a blank line looks like a broken key: it takes the walk from one empty entry
    // to the next, and the user cannot tell it apart from reaching the top.
    let mut screen = screen_with_commands();
    screen.seed_history(["real".to_string(), "   ".to_string(), String::new()]);
    assert_eq!(screen.history, vec!["real".to_string()]);
}

#[test]
fn the_caret_of_a_recalled_entry_lands_at_the_end() {
    // Recalling a line is for running or amending it, so the caret belongs where the
    // typing stopped — not at the start, where the next keystroke would be an insertion
    // into the middle of a command.
    let mut screen = screen_with_commands();
    screen.history = vec!["/name x".into()];
    set_input(&mut screen, "");
    screen.history_up();
    assert_eq!(screen.editing.as_ref().unwrap().caret(), 7);
}

#[test]
fn no_cursor_is_reported_when_not_editing() {
    // While a turn streams, the transcript owns the screen and the terminal caret has
    // nowhere to sit.
    let screen = screen_with_commands();
    let (_, cursor) = screen.compose_live();
    assert!(cursor.is_none());
}

#[test]
fn toggling_with_nothing_to_expand_says_so() {
    // Ctrl+O with no collapsible block on screen must not look like a dead key. The note
    // is pushed by the screen rather than by each caller, so this is where it is checked.
    let mut screen = screen();
    screen.push(Block::lines(vec![Line::plain("plain output")]));
    assert!(!screen.show_tool_details());
    assert_eq!(screen.notice.as_deref(), Some("没有工具详情"));
}

#[test]
fn detail_navigation_and_search_preserve_the_transcript_and_draft() {
    let mut screen = screen();
    let captured = CapturedOutput::default();
    screen.out = Box::new(captured.clone());
    screen.interactive = true;
    screen.push(Block::collapsible(
        (0..60).map(|n| Line::plain(format!("older {n}"))).collect(),
        1,
        0,
        0,
    ));
    screen.push(Block::lines(vec![Line::plain("later")]));
    screen.begin_stream();
    screen.push_text("tool commentary");
    screen.end_stream();
    screen.push(Block::collapsible(vec![Line::plain("newer")], 1, 0, 0));
    assert!(
        screen
            .blocks
            .iter()
            .flat_map(|block| block.render(40))
            .any(|line| line.text().contains("tool commentary"))
    );
    set_input(&mut screen, "draft");
    screen.absorb_event(Event::Key(KeyCode::Left.into()));
    let original = screen.editing.clone().unwrap();
    let mut terminal = vt100::Parser::new(nz(24), nz(40), 100);
    assert!(screen.show_tool_details());
    terminal.process(&captured.take());
    assert!(terminal.screen().contents().contains("newer"));
    assert!(!terminal.screen().contents().contains("tool commentary"));
    assert!(
        screen
            .blocks
            .iter()
            .all(|block| !matches!(block, Block::Collapsible(c) if c.expanded))
    );
    screen.absorb_event(Event::Key(KeyCode::Left.into()));
    for c in "/older 50".chars() {
        screen.absorb_event(Event::Key(KeyCode::Char(c).into()));
    }
    screen.absorb_event(Event::Key(KeyCode::Enter.into()));
    screen.flush();
    terminal.process(&captured.take());
    assert!(terminal.screen().contents().contains("older 50"));
    assert!(!terminal.screen().contents().contains("older 0"));
    screen.absorb_event(Event::Key(KeyCode::Esc.into()));
    screen.flush();
    terminal.process(&captured.take());
    assert!(screen.detail.is_none());
    assert!(terminal.screen().contents().contains("› draft"));
    assert!(terminal.screen().contents().contains("tool commentary"));
    assert_eq!(screen.editing.as_ref().unwrap().text(), original.text());
    assert_eq!(screen.editing.as_ref().unwrap().caret(), original.caret());
    assert_eq!(screen.blocks.len(), 4);

    let (mut log, path) = util::create_output_file().unwrap();
    writeln!(log, "early output").unwrap();
    screen.set_running(vec![vec![Span::plain("long command ".repeat(20))]]);
    screen.running_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
    screen.set_running_log(path.clone());
    assert!(util::strip_ansi(&screen.live_frame()).contains("3s"));
    assert!(
        screen
            .compose_live()
            .0
            .iter()
            .any(|row| row.line.text().contains("Ctrl+O"))
    );
    assert!(screen.show_tool_details());
    screen.flush();
    terminal.process(&captured.take());
    assert!(terminal.screen().contents().contains("early output"));
    writeln!(log, "later output").unwrap();
    screen.absorb_event(Event::Resize(41, 24));
    screen.flush();
    terminal.process(&captured.take());
    assert!(terminal.screen().contents().contains("later output"));
    screen.clear_running();
    screen.flush();
    terminal.process(&captured.take());
    assert!(screen.detail.is_none());
    assert_eq!(screen.editing.as_ref().unwrap().text(), original.text());
    assert_eq!(screen.editing.as_ref().unwrap().caret(), original.caret());
    assert_eq!(screen.blocks.len(), 4);
    drop(log);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn colored_output_keeps_its_text_without_synthetic_padding() {
    let screen = screen();
    let line = Line::spans(vec![Span::new(
        "added",
        Style::with_bg(Color::DiffAddedText, Bg::Added),
    )]);
    let painted = screen.paint(&line);
    assert_eq!(util::strip_ansi(&painted), "added");
    // Re-measuring the raw string would be wrong, which is the whole reason spans exist.
    assert!(painted.len() > 5);
}

#[test]
fn no_live_row_is_wider_than_the_terminal() {
    // A row wider than the screen is wrapped *by the terminal*, which knows nothing about
    // this code's row count. Every draw then erases one row fewer than it drew, the region
    // creeps down a row, and the old copy is left behind — the same line repeated down the
    // screen, broken off at the right edge. The model is not repeating itself; the screen is.
    //
    // So the invariant is checked directly: whatever `compose_live` hands to the terminal
    // has to fit inside the width it was composed for. Every part of the region is exercised,
    // because each one is a separate chance to forget the wrap.
    let mut screen = screen();
    screen.width = 40;
    screen.interactive = false;
    screen.begin_stream();
    screen.set_working(WORKING_LABEL);
    set_input(&mut screen, "draft");
    screen.set_commands(&[("model", "切换模型并选择思考级别")]);

    let long = "一段没有空格分隔的很长的中文内容会一直写到终端的右边然后被硬生生截断";
    let check = |screen: &Screen, what: &str| {
        let (lines, _) = screen.compose_live();
        for (index, line) in lines.iter().enumerate() {
            let width = line.line.width();
            assert!(
                width <= screen.width,
                "{what}: row {index} is {width} cells wide in a {} cell screen: {:?}",
                screen.width,
                line.line.text()
            );
        }
    };

    // The answer preview: long unbroken prose, a wide table, a long code line, a bare URL.
    for chunk in [
        "这是一段很长的中文回答，它会一路写到终端的右边并且在没有任何空格的地方被截断，",
        "然后继续写下去，让这一行远远超过四十列的宽度，",
        "\n\n| 列一 | 列二 | 列三 | 列四 | 列五 |\n| --- | --- | --- | --- | --- |\n",
        "| 一个很长的单元格内容 | 另一个很长的单元格 | 第三个很长的单元格 | x | y |\n\n",
        "```\nlet a_very_long_line = something_that_goes_on_and_on_and_on_beyond_the_edge();\n```\n",
        "https://example.com/一个非常长的没有空格的网址用来撑破右边",
    ] {
        screen.push_text(chunk);
        check(&screen, "answer preview");
    }

    // A queued line is user input, so it can be as long as a paste.
    screen.queue(Queued::new(long.into(), Vec::new()));
    check(&screen, "queued line");

    // The command menu and its help text.
    set_input(&mut screen, "/");
    screen.sync_menu();
    check(&screen, "menu");

    // The thinking preview is wrapped too, and one_line() has to run before it is.
    screen.push_thinking(&long.repeat(3));
    check(&screen, "thinking preview");

    // A notice: a failed paste carries the error text, which can be long.
    screen.notice = Some(long.repeat(2));
    check(&screen, "notice");

    // The running line is built from the command the model asked for, so it is as long as
    // the command is.
    screen.notice = None;
    screen.set_running(vec![
        vec![Span::plain(format!("● $ echo {long}"))],
        vec![Span::plain("● 读取 src/main.rs")],
        vec![Span::plain("● 搜索 src/ui")],
    ]);
    check(&screen, "running line");
    let running = screen.compose_live().0;
    for command in ["● $ echo", "● 读取 src/main.rs", "● 搜索 src/ui"] {
        assert_eq!(
            running
                .iter()
                .filter(|row| row.line.text().starts_with(command))
                .count(),
            1
        );
    }

    // The footer is produced by `footer::render`, which is width-aware, so what is checked
    // here is that the width it produced is the width the region gets. `set_footer` is fed
    // that output rather than a hand-built row, because a hand-built row would only be
    // testing the test.
    let mut state = FooterState {
        cwd: Path::new("/tmp/project").into(),
        branch: None,
        session_name: None,
        totals: crate::config::Usage {
            input: 173_000,
            output: 1_300,
            cache_read: 0,
            cache_write: 0,
        },
        cache_hit_rate: Some(99.5),
        output_rate: None,
        context_usage: crate::ui::footer::ContextUsage::Tokens(17_300),
        context_window: Some(1_000_000),
        model: None,
        level: "high".into(),
        busy: None,
    };
    screen.set_footer(crate::ui::footer::render(
        &state,
        &Theme::default(),
        screen.width,
    ));
    check(&screen, "footer");
    // A long branch and model name are the two fields that can outgrow the row.
    state.branch = Some(long.repeat(2));
    screen.set_footer(crate::ui::footer::render(
        &state,
        &Theme::default(),
        screen.width,
    ));
    check(&screen, "footer with a long branch");
}

#[test]
fn the_live_region_is_trimmed_to_the_screen() {
    // `trim_live` is what keeps the region from growing taller than the terminal. A region
    // taller than the screen cannot be erased by walking up from the caret: the rows above
    // the visible part are never reached, so they stay on screen and the frame is redrawn
    // over them — the transcript appears to freeze and repeat.
    //
    // (The row *widths* are `no_live_row_is_wider_than_the_terminal`'s business; this test
    // is only about the count, so it checks the count.)
    let mut screen = screen();
    screen.width = 40;
    screen.height = 12;
    screen.interactive = false;
    screen.begin_stream();
    screen.set_working(WORKING_LABEL);
    set_input(&mut screen, "draft");

    let mut previous = 0usize;
    for chunk in [
        "好的，我来说明一下。",
        "首先这一段会写得很长，长到超过四十列，因为它没有任何换行的机会，",
        "接着第二段同样很长，继续向右延伸下去直到远远越过边界，",
        "再来一段，把活区堆到超过终端的高度为止，这样 trim_live 必须动手",
        "```\nvery_long_code_line_without_any_break_points_here_at_all();\n```",
        "| 甲 | 乙 | 丙 | 丁 | 戊 | 己 |\n| --- | --- | --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 | 5 | 6 |\n",
        "最后一段，继续堆，确认行数不会越过屏幕高度。",
    ] {
        screen.push_text(chunk);
        let (lines, cursor) = screen.compose_live();
        assert!(
            lines.len() <= screen.height,
            "region grew to {} rows in a {} row screen (was {previous})",
            lines.len(),
            screen.height
        );
        // The caret has to be inside the region, or the next erase starts from a row that
        // is not part of the frame.
        if let Some((row, column)) = cursor {
            assert!(row < lines.len(), "the caret is outside the region");
            assert!(column <= screen.width, "the caret is past the right edge");
        }
        previous = lines.len();
    }
}

#[test]
fn the_composer_does_not_move_while_thinking_streams() {
    // While the model thinks, the composer stays armed and the user types into it. The
    // caret has to stay where they left it: a caret that wanders while text arrives
    // somewhere else is worse than no caret, because it is the one thing on screen that
    // claims to know where the next character goes.
    let mut screen = screen();
    screen.begin_stream();
    screen.set_working("Working");
    screen.editing = Some(Editor::from_text("打了一半的草稿"));

    let mut seen = Vec::new();
    for chunk in [
        "先看看 ",
        "这一步要做什么，",
        "然后动手。",
        "再看看边界条件。",
    ] {
        screen.push_thinking(chunk);
        let (_, caret) = screen.compose_live();
        seen.push(caret);
    }
    let first = seen[0];
    assert!(
        seen.iter().all(|caret| *caret == first),
        "the caret moved as thinking arrived: {seen:?}"
    );
    assert!(
        first.is_some(),
        "the composer is armed, so there is a caret"
    );
}

#[test]
fn the_thinking_preview_only_keeps_the_last_two_lines() {
    let mut screen = screen();
    screen.push_thinking("a\nb\nc\nd\ne");
    let (live, _) = screen.compose_live();
    assert!(live.len() <= 2, "{live:?}");
    assert!(live.iter().any(|line| line.line.text().contains('e')));

    // Earlier prose must not permanently suppress a later thinking segment, even when
    // that prose fills the viewport. The next answer hides it again without losing text.
    screen.push_text(&"answer\n".repeat(40));
    screen.push_thinking("\n\n");
    assert!(!screen.thinking_active);
    for chunk in [" first thought", " continuing thought", " latest thought"] {
        screen.push_thinking(chunk);
        let (live, _) = screen.compose_live();
        let thought_rows: Vec<_> = live
            .iter()
            .filter(|row| {
                row.line
                    .spans
                    .iter()
                    .any(|span| span.style.fg == Color::Dim)
            })
            .map(|row| row.line.text())
            .collect();
        assert!(thought_rows.len() <= 2);
        assert!(
            thought_rows.join(" ").contains(chunk.trim()),
            "{thought_rows:?}"
        );
    }
    screen.push_text("done");
    assert!(!screen.thinking_active);
    let (answer, thinking) = screen.end_stream();
    assert!(answer.ends_with("done"));
    assert!(thinking.ends_with("latest thought"));
    assert!(
        !screen
            .blocks
            .iter()
            .flat_map(|block| block.render(80))
            .any(|line| line.text().contains("latest thought"))
    );
    screen.begin_stream();
    screen.push_thinking("next call");
    assert!(screen.thinking_active);
    screen.discard_stream();
    assert!(!screen.thinking_active);
}

#[test]
fn ansi_in_streamed_text_is_neutralised_before_display() {
    let mut screen = screen();
    screen.streaming_answer = Some("\u{1b}[31mred\u{1b}[0m".into());
    let (live, _) = screen.compose_live();
    assert!(live.iter().all(|line| !line.line.text().contains('\u{1b}')));
}

/// A paste big enough to swamp the input area is folded to a one-line label, and the text
/// behind it is what gets sent.
///
/// Sized from the recorded sessions: the large pastes were a CI log (85 lines), a terminal
/// dump (47) and a diff (31). The input area shows at most eight rows, so those scrolled and
/// pushed the line being written out of sight.
#[test]
fn a_large_paste_is_folded_to_a_label_and_still_sent_whole() {
    let mut screen = screen();
    screen.begin_line();
    let log = (1..=40)
        .map(|line| format!("error[E0433]: line {line} of the CI log"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.absorb_event(Event::Paste(log.clone())).is_none());

    // One row on screen, however many lines went in.
    let (rows, _) = screen.compose_live();
    let shown: Vec<String> = rows.iter().map(|row| row.line.text()).collect();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(
        shown[0],
        format!("› [粘贴的内容 {} 字符]", log.chars().count())
    );

    // And the whole thing is what leaves on Enter, character for character.
    let action = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(
        matches!(action, Some(Action::Line(line)) if line == log),
        "the folded text must be sent, not the label"
    );
}

#[test]
fn the_fold_threshold_is_a_character_count_or_a_line_count() {
    let pasted = |text: String| {
        let mut screen = screen();
        screen.begin_line();
        assert!(screen.absorb_event(Event::Paste(text)).is_none());
        screen.editing.as_ref().unwrap().text()
    };
    // Under both bounds the text is shown as pasted: a short note is not a block.
    assert_eq!(pasted("短".repeat(20)), "短".repeat(20));
    assert_eq!(pasted("x".repeat(999)), "x".repeat(999));
    assert_eq!(pasted("a\nb\nc\nd\ne\nf\ng".into()), "a\nb\nc\nd\ne\nf\ng");
    // Either bound on its own folds: 85 short lines disturb the input area as much as one
    // long blob, and a single minified line is as tall on screen as it is wide.
    assert!(pasted("x".repeat(1000)).contains("粘贴的内容 1000 字符"));
    assert!(pasted("a\nb\nc\nd\ne\nf\ng\nh".into()).contains("粘贴"));
}

#[test]
fn text_written_around_a_folded_paste_is_sent_around_its_text() {
    let mut screen = screen();
    screen.begin_line();
    let block = "块".repeat(1200);
    assert!(screen.absorb_event(Event::Paste(block.clone())).is_none());
    // Type in front of the label, which is where a user explains what they pasted.
    screen.editing.as_mut().unwrap().home();
    screen.editing.as_mut().unwrap().insert("请看这个报错：");
    let action = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let Some(Action::Line(line)) = action else {
        panic!("the line must be sent: {action:?}");
    };
    assert_eq!(line, format!("请看这个报错：{block}"));
}

#[test]
fn a_label_the_user_typed_is_not_expanded() {
    // Only what this editor set aside is put back. A label that was typed by hand has no text
    // behind it, and expanding it would invent content the user never pasted.
    let mut screen = screen();
    screen.begin_line();
    screen
        .editing
        .as_mut()
        .unwrap()
        .insert("[粘贴的内容 100 字符]");
    let action = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let Some(Action::Line(line)) = action else {
        panic!("the line must be sent: {action:?}");
    };
    assert_eq!(line, "[粘贴的内容 100 字符]");
}

#[test]
fn deleting_a_folded_label_deletes_its_text_too() {
    // A stand-in that was removed is not a stand-in any more: leaving the text set aside would
    // send content the user deliberately deleted.
    let mut screen = screen();
    screen.begin_line();
    screen.absorb_event(Event::Paste("q".repeat(1200)));
    screen.editing.as_mut().unwrap().home();
    for _ in 0..40 {
        screen.editing.as_mut().unwrap().delete();
    }
    assert_eq!(screen.editing.as_ref().unwrap().text(), "");
    let action = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    // Nothing goes out: the text went with the label that was deleted.
    let Some(Action::Line(line)) = action else {
        panic!("empty line: {action:?}");
    };
    assert_eq!(line, "");
}

#[test]
fn recalling_a_folded_line_shows_the_label_again_and_still_sends_the_text() {
    // History holds the line as sent, because that is what re-sending reproduces. Showing it
    // again is what would put 40 lines back on the input area.
    let mut screen = screen();
    screen.begin_line();
    let block = "r".repeat(2000);
    screen.absorb_event(Event::Paste(block.clone()));
    let action = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(matches!(action, Some(Action::Line(line)) if line == block));

    // Up recalls it: the label is back, and Enter sends the text again.
    screen.begin_line();
    screen.absorb_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
    assert_eq!(
        screen.editing.as_ref().unwrap().text(),
        "[粘贴的内容 2000 字符]"
    );
    let again = screen.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(
        matches!(again, Some(Action::Line(line)) if line == block),
        "a recalled line must send what it sent before"
    );
}

/// A path pasted at the prompt attaches the image instead of inserting its text.
///
/// This is the only way to send a picture when there is no display server: `Ctrl+V` reads
/// the clipboard through X11 or Wayland, and a headless session has neither.
#[test]
fn pasting_an_image_path_attaches_it_and_pasting_text_still_inserts_text() {
    let dir = std::env::temp_dir().join(format!("pi-imgpaste-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 8, 4);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0u8; 8 * 4 * 4]).unwrap();
        writer.finish().unwrap();
        std::fs::write(&png, bytes).unwrap();
    }

    let mut s1 = screen();
    s1.begin_line();
    s1.attach_text_or_image(&png.to_string_lossy());
    assert_eq!(s1.pending_images.len(), 1, "the file is attached");
    assert_eq!(input(&s1), "", "the path is not inserted as text");

    // Ordinary text still goes into the buffer, and the earlier image stays attached.
    s1.attach_text_or_image("照着这张图改");
    assert_eq!(input(&s1), "照着这张图改");
    assert_eq!(s1.pending_images.len(), 1);

    // A real file that is not a PNG says so instead of pasting the path as text.
    let txt = dir.join("note.txt");
    std::fs::write(&txt, "hello").unwrap();
    let mut s2 = screen();
    s2.begin_line();
    s2.attach_text_or_image(&txt.to_string_lossy());
    assert_eq!(
        input(&s2),
        "",
        "a non-PNG file is not pasted as text either"
    );
    assert!(
        s2.notice
            .as_deref()
            .is_some_and(|n| n.contains("没有附加图片")),
        "{:?}",
        s2.notice
    );

    // A path that does not exist is just text.
    let mut s3 = screen();
    s3.begin_line();
    s3.attach_text_or_image("/no/such/file.png");
    assert_eq!(input(&s3), "/no/such/file.png");
    std::fs::remove_dir_all(dir).unwrap();
}

/// The clipboard route, with each branch driven directly.
///
/// The readers are injected precisely so this is testable: `arboard` cannot succeed without a
/// display server, so a test that called the real ones would only ever exercise the failure
/// path — and the bug being guarded against was a failure path that did *not* fall through.
#[test]
fn every_clipboard_outcome_lands_somewhere_useful() {
    use crate::image_input::{ImageError, PastedImage};

    let dir = std::env::temp_dir().join(format!("pi-clip-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 4, 4);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0u8; 4 * 4 * 4]).unwrap();
        writer.finish().unwrap();
        std::fs::write(&png, bytes).unwrap();
    }
    let path_text = png.to_string_lossy().to_string();

    // 1. An image on the clipboard wins.
    let mut a = screen();
    a.begin_line();
    a.paste_from_clipboard(
        || {
            Ok(PastedImage {
                width: 2,
                height: 2,
                data: "AA==".into(),
                media_type: "image/png",
                bytes: 3,
            })
        },
        || Ok("ignored text".into()),
    );
    assert_eq!(a.pending_images.len(), 1);
    assert_eq!(input(&a), "", "text must not be inserted when an image won");

    // 2. The clipboard cannot be opened at all — the headless case, and the one that was
    // broken. It must still reach the path route.
    let mut b = screen();
    b.begin_line();
    b.paste_from_clipboard(
        || Err(ImageError::Clipboard("no display server".into())),
        || Ok(path_text.clone()),
    );
    assert_eq!(
        b.pending_images.len(),
        1,
        "a clipboard that will not open must still reach the path route: {:?}",
        b.notice
    );
    assert_eq!(input(&b), "", "the path is not inserted as text");

    // 3. Text that is not a path is pasted as text.
    let mut c = screen();
    c.begin_line();
    c.paste_from_clipboard(|| Err(ImageError::NoImage), || Ok("普通的文字".into()));
    assert_eq!(input(&c), "普通的文字");
    assert!(c.pending_images.is_empty());

    // 4. Neither an image nor text: report why, rather than doing nothing silently.
    let mut d = screen();
    d.begin_line();
    d.paste_from_clipboard(
        || Err(ImageError::Clipboard("no display server".into())),
        || Err(ImageError::NoText),
    );
    assert_eq!(input(&d), "");
    assert!(
        d.notice.as_deref().is_some_and(|n| n.contains("没有显示")
            || n.contains("no display")
            || n.contains("粘贴失败")),
        "{:?}",
        d.notice
    );

    std::fs::remove_dir_all(dir).unwrap();
}

/// Typing a path to a PNG and pressing Enter sends the picture.
///
/// This is the route that needs no clipboard at all, which is what makes it the only one
/// available on a machine with no display server.
#[test]
fn submitting_a_path_sends_the_image_instead_of_the_text() {
    let dir = std::env::temp_dir().join(format!("pi-enterpath-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 6, 3);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0u8; 6 * 3 * 4]).unwrap();
        writer.finish().unwrap();
        std::fs::write(&png, bytes).unwrap();
    }
    let path_text = png.to_string_lossy().to_string();

    // A bare path becomes an image, and the text goes along as the message.
    let mut s1 = screen();
    s1.begin_line();
    set_input(&mut s1, &path_text);
    let action = s1.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let Some(Action::LineWithImages(line, images)) = action else {
        panic!("expected the picture to travel with the line: {action:?}");
    };
    assert_eq!(line, path_text, "the path still reads as the message");
    assert_eq!(images.len(), 1, "the file is attached");
    assert_eq!((images[0].width, images[0].height), (6, 3));

    // Prose that merely mentions the path is sent as written, with nothing attached.
    let mut s2 = screen();
    s2.begin_line();
    set_input(&mut s2, &format!("看看 {path_text} 这个报错"));
    let action = s2.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(
        matches!(action, Some(Action::Line(ref line)) if line.contains("这个报错")),
        "a sentence is not a path: {action:?}"
    );

    // A file that exists but is not an image at all is refused with a reason, and the line
    // stays put: sending the path as text would leave the user thinking the picture went
    // with it.
    let jpeg = dir.join("notes.txt");
    std::fs::write(&jpeg, b"just text, not an image").unwrap();
    let mut s4 = screen();
    s4.begin_line();
    set_input(&mut s4, &jpeg.to_string_lossy());
    let refused = s4.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(refused.is_none(), "nothing is sent: {refused:?}");
    assert!(
        s4.notice.as_deref().is_some_and(|n| n.contains("图片格式")),
        "{:?}",
        s4.notice
    );
    assert_eq!(
        input(&s4),
        jpeg.to_string_lossy(),
        "the line is kept so it can be corrected"
    );

    // A path that does not exist is just text.
    let mut s3 = screen();
    s3.begin_line();
    set_input(&mut s3, "/no/such/shot.png");
    let action = s3.absorb_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert!(matches!(action, Some(Action::Line(_))), "{action:?}");

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn caret_layout_handles_midline_unicode_and_wrap_boundaries() {
    for (text, width, back, expected_row, expected_column, expected_line) in [
        ("helo world", 40, 6, 0, 6, "› helo world"),
        ("你好", 40, 1, 0, 4, "› 你好"),
        ("你好", 40, 0, 0, 6, "› 你好"),
        ("abcdefghijklm", 12, 3, 1, 2, "  klm"),
        ("abcdefghij", 12, 0, 1, 2, "  "),
        ("abcdefghijklmnopqrstuvwxyz", 12, 0, 2, 8, "  uvwxyz"),
    ] {
        let mut screen = screen_with_commands();
        screen.width = width;
        set_input(&mut screen, text);
        caret_back(&mut screen, back);
        let (lines, cursor) = screen.compose_live();
        assert_eq!(cursor, Some((expected_row, expected_column)), "{text}");
        assert_eq!(lines[expected_row].line.text(), expected_line, "{text}");
    }
}
