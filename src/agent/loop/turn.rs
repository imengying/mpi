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
            // Threshold compaction runs before the request, using real usage when it is still
            // valid and an estimate otherwise.
            if let Some(window) = model.context_window {
                let limit = compact::threshold_for(window);
                let real_usage = self.session.last_usage.as_ref().map(|usage| {
                    usage.input + usage.output + usage.cache_read + usage.cache_write
                });
                let used = compact::estimate_context(
                    &messages,
                    self.system_prompt.as_deref().unwrap_or_default(),
                    real_usage,
                );
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
            let tools = tools::specs();
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
                Some(Ok(completion)) => completion,
                Some(Err(err)) => {
                    // Nothing is committed, so the discarded preview leaves no trace.
                    self.screen.discard_stream();
                    if let Some(retried) = self.recover_overflow(&err.message()).await? {
                        return Ok(retried);
                    }
                    return Err(err.into());
                }
            };
            self.screen.end_stream();

            // An overflow can also arrive as a "successful" response: either the prompt alone
            // filled the window, or the server truncated it and left no room to answer. Both
            // are compacted here, before the message is recorded. No note: the compaction puts
            // its own banner in the footer, and it says exactly what happened.
            let overflow = compact::detect_overflow(&completion, model.context_window, model.max_tokens());
            if overflow.is_some() {
                let retried = self.recover_overflow("").await?;
                if let Some(end) = retried {
                    return Ok(end);
                }
                // A response that finished successfully cannot be continued by resending, so
                // the compaction alone is the recovery; nothing else to retry.
                return Ok(TurnEnd::Done);
            }

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
            self.session.push_token_count(completion.usage)?;
            self.compaction.observe_usage();
            self.render_footer(None);

            if let Some(error) = &completion.error {
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("上游返回错误：{error}"),
                    crate::ui::screen::Style::new(Color::Red),
                ));
                return Ok(TurnEnd::Done);
            }
            Ok(self.after_completion(&completion))
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
        pub(super) fn after_completion(&mut self, completion: &llm::Completion) -> TurnEnd {
            let calls = completion.tool_calls();
            // A hosted search that paused has to be sent back as-is. There is no local call
            // to run; doing so would invent a tool result the server did not ask for.
            if calls.is_empty() && completion.stop_reason == StopReason::Pause {
                return TurnEnd::Continue;
            }
            if calls.is_empty() {
                // A length stop with no tool calls means the answer was cut off; say so rather
                // than silently pretending it finished. "Continue" needs no spelling out.
                if completion.stop_reason == StopReason::Length {
                    self.screen.push_lines(ui_compact::note_lines(
                        "输出达到长度上限，回答可能不完整。",
                        crate::ui::screen::Style::new(Color::Yellow),
                    ));
                }
                return TurnEnd::Done;
            }
            // Tool calls are executed inline; the next request carries their results. Esc during
            // the calls ends the turn here: the results already produced are in the session, and
            // carrying on would run the very thing the user just stopped.
            let stopped = self.execute_tools(&calls);
            if stopped {
                return TurnEnd::Stopped;
            }
            TurnEnd::Continue
        }
}
