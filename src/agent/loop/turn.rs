//! Driving one turn: the request, the stream, and what happens after it ends.

use super::*;

impl Agent {

        /// Run one user turn to completion.
        pub async fn run_turn(&mut self, input: &str) -> anyhow::Result<()> {
            self.run_turn_with_images(input, Vec::new()).await
        }


        /// A turn with pasted images attached.
        ///
        /// The images and the text become **one** user message: splitting them would let the
        /// model answer the text without having seen the picture, which is the exact mistake a
        /// screenshot is meant to prevent.
        pub async fn run_turn_with_images(
            &mut self,
            input: &str,
            images: Vec<crate::image_input::PastedImage>,
        ) -> anyhow::Result<()> {
            self.screen.collapse_all();
            self.screen.push_lines(ui_compact::user_lines(input));
            for image in &images {
                self.screen.push_lines(ui_compact::note_lines(
                    &image.label(),
                    crate::ui::screen::Style::new(Color::Magenta),
                ));
            }
            let mut content: Vec<crate::llm::Block> = Vec::new();
            if !input.is_empty() {
                content.push(crate::llm::Block::Text { text: input.to_string() });
            }
            content.extend(images.iter().map(crate::image_input::PastedImage::block));
            self.session
                .push_message(Message::User { content }, None, None)?;
            // Record the model and level this turn is about to run with, so resuming the session
            // continues with the model the user chose rather than the one the session happened to
            // be created with. It is written per turn because the choice can change between them:
            // `/model` mid-conversation has to be remembered, and the newest record is the one
            // that answers "what was this session using".
            self.session.push_turn_context(&self.cwd, &self.model_spec, &self.level)?;
            self.retry.reset();
            // The spinner covers the whole turn, not one request: the model may think for a
            // while before its first token, and a command may run for minutes. Both are times
            // when nothing else on screen moves, and a mark that does not move cannot be told
            // apart from a process that has hung.
            self.screen.set_working(WORKING_LABEL);

            loop {
                match self.assistant_turn().await {
                    Ok(TurnEnd::Done) => break,
                    Ok(TurnEnd::Continue) => continue,
                    // Esc during a tool call: the results produced so far are already recorded,
                    // and the turn is over. Without the stop the loop would send them and ask the
                    // model for the next step — of the very turn that was just stopped.
                    Ok(TurnEnd::Stopped) => {
                        self.screen.push_lines(ui_compact::note_lines(
                            "已停止",
                            crate::ui::screen::Style::new(Color::Dim),
                        ));
                        break;
                    }
                    Err(err) => {
                        self.screen.push_lines(ui_compact::note_lines(
                            &format!("请求失败：{err}"),
                            crate::ui::screen::Style::new(Color::Red),
                        ));
                        break;
                    }
                }
            }
            // A finished task leaves every block collapsed and everything committed.
            self.screen.collapse_all();
            self.screen.clear_working();
            self.render_footer(None);
            Ok(())
        }


        /// One request/response cycle, including its tool calls.
        pub(super) async fn assistant_turn(&mut self) -> anyhow::Result<TurnEnd> {
            let (provider, model, compat) = {
                let (provider, model) = self.model()?;
                let compat = provider.compat(model);
                (provider.clone(), model.clone(), compat)
            };
            // Built once. The threshold check below only reads it; cloning the history a
            // second time would copy every image in the context on the common path, where
            // compaction does not fire. After a compaction the list is stale and is rebuilt.
            let mut messages = self.session.context_messages();
            let tools = tools::specs();
            // Threshold compaction runs before the request, using real usage when it is still
            // valid and an estimate otherwise.
            if let Some(window) = model.context_window {
                let limit = compact::threshold_for(window, model.max_tokens());
                let real_usage = self.session.measured_context_tokens();
                let used = compact::estimate_context(
                    &messages,
                    self.system_prompt.as_deref().unwrap_or_default(),
                    real_usage,
                );
                let used = used + if real_usage.is_none() {
                    tools.iter().map(|tool| crate::util::estimate_tokens(&serde_json::to_string(tool).expect("tool schema"))).sum::<u64>()
                } else { 0 };
                if used > limit {
                    // No note here: the footer's own banner already says the context is near its
                    // limit and a compaction is running. It is the same sentence, on the row the
                    // user is looking at while the request is in flight.
                    if let Err(err) = self.compact(Reason::Threshold, None).await {
                        // A failed automatic compaction must not lose the turn; the request
                        // may still fit, and if it does not the overflow path will try again.
                        self.screen.push_lines(ui_compact::note_lines(
                            &format!("自动压缩未完成：{err}"),
                            crate::ui::screen::Style::new(Color::Yellow),
                        ));
                    }
                    messages = self.session.context_messages();
                }
            }

            // The system message is prepended per request rather than stored in the session:
            // the file stays a record of the conversation itself, and the prompt can be
            // changed on disk without rewriting history.
            if let Some(prompt) = &self.system_prompt {
                messages.insert(0, Message::System { content: prompt.clone() });
            }
            // Some gateways reject a history that ends with a tool result.
            if compat.requires_assistant_after_tool_result
                && matches!(messages.last(), Some(Message::Tool { .. }))
            {
                messages.push(Message::assistant_text("继续。"));
            }
            let level = self.level.clone();
            let session_id = self.session.header().id.clone();
            let request = Request {
                model: &model,
                provider: &provider,
                messages: &messages,
                tools: &tools,
                level: &level,
                session_id: &session_id,
                cache_hints: true,
            };

            self.streaming = true;
            self.screen.begin_stream();
            // Deltas travel through a channel rather than straight into the screen, because the
            // spinner has to be advanced from the same place: the sink cannot keep hold of the
            // screen across the `select` below, and a model that has not produced its first
            // token yet is exactly when the spinner matters.
            let (sender, mut deltas) = tokio::sync::mpsc::unbounded_channel::<Delta>();
            let outcome = {
                let mut sink = |delta: Delta| {
                    let _ = sender.send(delta);
                };
                let stream = self.client.stream(&request, &mut sink);
                tokio::pin!(stream);
                let mut ticker = ticker();
                // Esc ends the request by dropping the future, which closes the connection. The
                // tokens already received stay on screen: the user asked for the turn to stop,
                // not for what the model already said to be thrown away.
                let mut stop_requested = false;
                let mut result = None;
                loop {
                    // Keep the input line alive while the answer arrives. The user types into
                    // the composer as they read; Enter there queues the message rather than
                    // dropping it, because starting a second turn underneath this one would
                    // interleave two conversations.
                    if let Some(action) = self.screen.poll_input()
                        && on_turn_action(&mut self.screen, action)
                    {
                        stop_requested = true;
                    }
                    if stop_requested {
                        break;
                    }
                    tokio::select! {
                        // A token outranks the spinner: text has to appear as it arrives, not on
                        // the next frame. Everything already queued is drained with it, so a burst
                        // of tokens costs one redraw rather than one per token.
                        biased;
                        Some(delta) = deltas.recv() => {
                            apply_deltas(&mut self.screen, delta, &mut deltas);
                        }
                        _ = ticker.tick() => self.screen.tick_working(),
                        out = &mut stream => {
                            result = Some(out);
                            break;
                        }
                    }
                }
                drain_deltas(&mut self.screen, &mut deltas);
                TurnOutcome { result }
            };
            self.streaming = false;
            // A stop keeps the half-written answer, because it is a real answer the user chose to
            // cut short; a failure discards it, because a half-written answer that was never
            // recorded must not stay on screen.
            let completion = match outcome.result {
                None => {
                    debug_assert!(outcome.stopped(), "no completion means the stream was stopped");
                    return self.finish_stopped().map(|()| TurnEnd::Done);
                }
                Some(Ok(completion)) => completion.checked(),
                Some(Err(err)) => {
                    // Nothing is committed, so the discarded preview leaves no trace.
                    self.screen.discard_stream();
                    if let Some(retried) = self.recover_overflow(&err.message()).await? {
                        return Ok(retried);
                    }
                    return Err(err.into());
                }
            };
            // An overflow can also arrive as a "successful" response: either the prompt alone
            // filled the window, or the server truncated it and left no room to answer. Both
            // are compacted here, before the message is recorded. No note: the compaction puts
            // its own banner in the footer, and it says exactly what happened.
            let overflow = compact::detect_overflow(&completion, model.context_window);
            if completion.stop_reason != StopReason::Length && overflow.is_some() && completion.text().trim().is_empty() {
                self.screen.discard_stream();
                let retried = self.recover_overflow("").await?;
                if let Some(end) = retried {
                    return Ok(end);
                }
                // A response that finished successfully cannot be continued by resending, so
                // the compaction alone is the recovery; nothing else to retry.
                return Ok(TurnEnd::Done);
            }

            self.screen.end_stream();
            let citations = llm::citation_lines(&completion.message);
            if !citations.is_empty() {
                let mut lines = Vec::new();
                for line in &citations {
                    lines.push(crate::ui::screen::Line::new(
                        line.clone(),
                        crate::ui::screen::Style::new(Color::Dim),
                    ));
                }
                lines.push(crate::ui::screen::Line::blank());
                self.screen.push_lines(lines);
            }

            self.session
                .push_message(completion.message.clone(), Some(completion.usage), Some(completion.stop_reason))?;
            self.compaction.observe_usage();
            self.render_footer(None);

            if let Some(error) = &completion.error {
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("上游返回错误：{error}"),
                    crate::ui::screen::Style::new(Color::Red),
                ));
                return Ok(TurnEnd::Done);
            }
            self.after_completion(&completion)
        }


        /// Close out a turn the user stopped with Esc.
        ///
        /// The partial answer is kept, not discarded: it is real output the model produced, and
        /// the next turn reads it as its own previous message — which is exactly what makes
        /// "stop, then say what you actually meant" work. It is recorded with a marker so a
        /// resumed session can tell "the user cut this off" from "the model finished here".
        pub(super) fn finish_stopped(&mut self) -> anyhow::Result<()> {
            let (answer, thinking) = self.screen.end_stream();
            let mut content: Vec<llm::Block> = Vec::new();
            if !thinking.trim().is_empty() {
                // The preview was rendered from the stream; reusing those bytes keeps the stored
                // block identical to what the user saw.
                content.push(llm::Block::Thinking { thinking, signature: None });
            }
            if !answer.trim().is_empty() {
                content.push(llm::Block::Text { text: answer });
            }
            self.session.push_message(
                Message::Assistant { content, stop_reason: Some(StopReason::Aborted) },
                None,
                Some(StopReason::Aborted),
            )?;
            // The partial answer is on screen above this line, so "已停止" is enough to account
            // for why it ends mid-sentence; how to carry on needs no instruction.
            self.screen.push_lines(ui_compact::note_lines(
                "已停止",
                crate::ui::screen::Style::new(Color::Dim),
            ));
            self.render_footer(None);
            Ok(())
        }


        /// Decide what to do after a response that is not an overflow.
        pub(super) fn after_completion(&mut self, completion: &llm::Completion) -> anyhow::Result<TurnEnd> {
            if completion.stop_reason == StopReason::Error || completion.error.is_some() {
                return Ok(TurnEnd::Done);
            }
            // A max-token finish is terminal, as in harness: preserve the partial output,
            // but do not invent another user turn or execute a potentially cut tool call.
            if completion.stop_reason == StopReason::Length {
                let note = if completion.text().trim().is_empty() && !completion.message.thinking().trim().is_empty() {
                    "思考达到输出上限，未产生正文或完整工具调用。可在 /model 调低思考级别后重试。"
                } else {
                    "输出达到上限，已保留生成内容；需要时可继续提问。"
                };
                self.screen.push_lines(ui_compact::note_lines(note, crate::ui::screen::Style::new(Color::Yellow)));
                return Ok(TurnEnd::Done);
            }
            let calls = completion.tool_calls();
            if calls.is_empty() {
                return Ok(if completion.stop_reason == StopReason::Pause {
                    TurnEnd::Continue
                } else { TurnEnd::Done });
            }
            // Tool calls are executed inline; the next request carries their results. Esc during
            // the calls ends the turn here: the results already produced are in the session, and
            // carrying on would run the very thing the user just stopped.
            let stopped = self.execute_tools(&calls)?;
            if stopped {
                return Ok(TurnEnd::Stopped);
            }
            Ok(TurnEnd::Continue)
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};

    #[tokio::test(flavor = "multi_thread")]
    async fn premature_eof_and_transport_failures_preserve_prose_without_executing_tools() {
        for failure in ["eof", "transport", "in-band", "decode"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                let event = serde_json::json!({"choices":[{"delta":{"content":"保留正文", "reasoning_content":"保留思考", "tool_calls":[{
                    "index":0,"id":"unconfirmed","function":{"name":"write","arguments":"{\"path\":\"must-not-exist\",\"content\":\"bad\"}"}
                }]}}]});
                let mut body = format!("data: {event}\n\n");
                if failure == "in-band" {
                    body.push_str("data: {\"error\":{\"message\":\"upstream failed\"}}\n\n");
                }
                if failure == "decode" {
                    body.push_str("data: broken-json\n\n");
                }
                let declared = body.len() + if failure == "transport" { 100 } else { 0 };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{body}").unwrap();
                socket.flush().unwrap();
            });
            let config: Config = serde_json::from_value(serde_json::json!({
                "providers":[{"name":"test","api":"completions","base_url":format!("http://{address}/v1"), "api_key":"local-test", "models":[{"id":"m","context_window":128000}]}],
                "default_model":"test/m"
            })).unwrap();
            let dir = std::env::temp_dir().join(format!("pi-unconfirmed-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&dir).unwrap();
            let mut agent = Agent::new(config, dir.clone(), false).unwrap();
            agent.client = Client::local_test_client();
            agent.session = Session::create_in(&dir, &dir, "test/m").unwrap();
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                agent.run_turn("处理任务"),
            )
            .await
            .unwrap()
            .unwrap();
            server.join().unwrap();
            assert!(!dir.join("must-not-exist").exists(), "{failure}");
            let messages = agent.session.context_messages();
            llm::validate_tool_history(&messages).unwrap();
            let assistant = messages
                .iter()
                .find(|m| matches!(m, Message::Assistant { .. }))
                .unwrap();
            assert_eq!(assistant.text(), "保留正文", "{failure}");
            assert_eq!(assistant.thinking(), "保留思考", "{failure}");
            assert!(assistant.tool_calls().is_empty());
            assert!(matches!(
                assistant,
                Message::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            ));
            drop(agent);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn length_cut_preserves_text_ends_the_turn_and_never_executes_cut_tool_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "missing model request");
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(err) => panic!("{err}"),
                    }
                };
                socket.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" { break; }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    { length = value.trim().parse::<usize>().unwrap(); }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                requests.push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                let delta = serde_json::json!({"content":"第一部分", "tool_calls":[{
                    "index":0,"id":"cut-call","function":{"name":"write",
                    "arguments":"{\"path\":\"must-not-exist\",\"content\":\"bad\"}"}
                }]});
                let event = serde_json::json!({"choices":[{"delta":delta,"finish_reason":"length"}],
                    "usage":{"prompt_tokens":100,"completion_tokens":128}});
                let body = format!("data: {event}\n\ndata: [DONE]\n\n");
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            (requests, listener)
        });
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":format!("http://{address}/v1"),
                "api_key":"local-test","models":[{"id":"m","max_tokens":128,"context_window":128000}]}],
            "default_model":"test/m"
        })).unwrap();
        let dir = std::env::temp_dir().join(format!("pi-continuation-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let mut agent = Agent::new(config, dir.clone(), false).unwrap();
        agent.client = Client::local_test_client();
        agent.session = Session::create_in(&dir, &dir, "test/m").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), agent.run_turn("完成任务"))
            .await.unwrap().unwrap();
        let (requests, listener) = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(!dir.join("must-not-exist").exists());
        let messages = agent.session.context_messages();
        assert_eq!(messages.iter().filter(|m| matches!(m, Message::Assistant { .. })).count(), 1);
        assert!(messages.iter().all(|m| m.tool_calls().is_empty()));
        assert!(messages.iter().any(|m| m.text() == "第一部分"));
        assert_eq!(agent.session.user_history(), vec!["完成任务"]);
        assert_eq!(agent.session.totals.input, 100, "one usage record per response");
        assert_eq!(agent.session.totals.output, 128);
        let level = agent.level.clone();
        let exhausted = llm::Completion {
            message: Message::Assistant { content:vec![llm::Block::Thinking {
                thinking:"没有产出正文的思考".into(), signature:None,
            }], stop_reason:Some(StopReason::Length) },
            usage:crate::config::Usage::default(), stop_reason:StopReason::Length, error:None,
        };
        assert!(matches!(agent.after_completion(&exhausted).unwrap(), TurnEnd::Done));
        assert_eq!(agent.level, level, "do not silently downgrade the selected effort");
        drop(agent);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
