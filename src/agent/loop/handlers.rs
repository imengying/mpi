//! Running the tools the model asked for, and what to do when they hurt.

use super::*;

impl Agent {
    /// Whether the protocol the current model speaks sends thinking back to the model.
    ///
    /// Both callers of [`Self::context_tokens`] run only after `context_window` was found,
    /// which already implies the model resolved; the `Dropped` fallback is therefore not
    /// reachable from a live request and exists to keep this total.
    pub(super) fn thinking_replay(&self) -> llm::ThinkingReplay {
        self.model()
            .map(|(provider, _)| llm::ThinkingReplay::for_provider(provider))
            .unwrap_or(llm::ThinkingReplay::Dropped)
    }

    pub(super) fn context_tokens(&self, messages: &[Message], tools: &[llm::ToolSpec]) -> u64 {
        let replay = self.thinking_replay();
        if let Some(used) = self.session.measured_context_tokens(replay) {
            return used;
        }
        llm::estimate_request_context(
            messages,
            self.system_prompt.as_deref().unwrap_or_default(),
            tools,
            replay,
        )
    }

    pub(super) fn prune_context(&mut self) -> anyhow::Result<bool> {
        if let Some(outcome) =
            compact::prune_tool_results(&self.session.context_messages(), self.session.path())
        {
            self.session.push_pruning(outcome)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Only adjacent read-only calls overlap. Results are persisted in model-call order.
    pub(super) async fn execute_tools(
        &mut self,
        calls: &[(String, String, serde_json::Value)],
    ) -> anyhow::Result<bool> {
        let mut next = 0;
        while next < calls.len() {
            let stopped = self
                .screen
                .poll_input()
                .is_some_and(|action| on_turn_action(&mut self.screen, action));
            if stopped || self.screen.has_steering() {
                self.skip_tools(&calls[next..], stopped)?;
                return Ok(stopped);
            }
            let first = &calls[next];
            let count = if tools::execution_mode(&first.1) == tools::ExecutionMode::Parallel {
                calls[next..]
                    .iter()
                    .take(4)
                    .take_while(|(_, name, _)| {
                        tools::execution_mode(name) == tools::ExecutionMode::Parallel
                    })
                    .count()
            } else {
                1
            };
            let batch = &calls[next..next + count];
            let mut running = ui_compact::running_line(&first.1, &first.2);
            if count > 1 {
                running.push(crate::ui::screen::Span::new(
                    format!("（另 {} 项）", count - 1),
                    crate::ui::screen::Style::new(Color::Dim),
                ));
            }
            self.render_footer(None);
            self.screen.set_running(running.clone());
            self.screen.suspend_live();
            // Every call keeps its own authorization decision; no batch-wide grant.
            let approved: Vec<_> = batch
                .iter()
                .map(|(_, name, arguments)| {
                    self.gate
                        .check(name, arguments, &self.cwd)
                        .map_err(|refusal| {
                            ToolOutput::error_for(name, arguments, refusal.message())
                        })
                })
                .collect();
            let started: Vec<_> = approved.iter().map(Result::is_ok).collect();
            self.screen.set_running(running);
            for ((id, name, _), approval) in batch.iter().zip(approved.iter()) {
                if let Ok(arguments) = approval {
                    self.session.push_tool_start(
                        id,
                        name,
                        arguments.clone(),
                        tools::replay_safe(name),
                    )?;
                }
            }
            let (results, stopped) = self.run_tool_batch(batch, approved).await;
            self.screen.clear_running();
            for (((id, name, arguments), (output, status)), started) in
                batch.iter().zip(results).zip(started)
            {
                let duration_ms = output
                    .duration
                    .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64);
                if started {
                    self.session.push_tool_end(id, name, status, duration_ms)?;
                }
                self.session.push_message(
                    Message::Tool {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        content: output.content.clone(),
                        status,
                    },
                    None,
                    None,
                )?;
                self.screen
                    .push(ui_compact::tool_block(name, arguments, &output, status));
            }
            next += count;
            if stopped {
                self.skip_tools(&calls[next..], true)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn skip_tools(
        &mut self,
        calls: &[(String, String, serde_json::Value)],
        stopped: bool,
    ) -> anyhow::Result<()> {
        let content = if stopped {
            "用户停止了本轮，这个调用没有执行。"
        } else {
            "收到用户补充，这个调用尚未执行；请结合最新要求重新决定操作。"
        };
        for (id, name, _) in calls {
            self.session.push_message(
                Message::Tool {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    content: content.into(),
                    status: ToolStatus::Skipped,
                },
                None,
                None,
            )?;
        }
        Ok(())
    }

    async fn run_tool_batch(
        &mut self,
        calls: &[(String, String, serde_json::Value)],
        approved: Vec<Result<serde_json::Value, ToolOutput>>,
    ) -> (Vec<(ToolOutput, ToolStatus)>, bool) {
        let mut workers = tokio::task::JoinSet::new();
        let mut task_indices = std::collections::HashMap::new();
        let mut results = vec![None; calls.len()];
        let context = tools::ToolContext {
            cwd: self.cwd.clone(),
            shell: self.config.shell.path.clone(),
        };
        for (index, approval) in approved.into_iter().enumerate() {
            match approval {
                Err(output) => results[index] = Some((output, ToolStatus::Error)),
                Ok(arguments) => {
                    let name = calls[index].1.clone();
                    let context = context.clone();
                    let task = workers.spawn(async move {
                        (index, tools::execute(&name, &arguments, &context).await)
                    });
                    task_indices.insert(task.id(), index);
                }
            }
        }
        let mut ticker = ticker();
        let mut stopped = false;
        while !workers.is_empty() {
            if self
                .screen
                .poll_input()
                .is_some_and(|action| on_turn_action(&mut self.screen, action))
            {
                stopped = true;
                break;
            }
            tokio::select! {
                result = workers.join_next() => {
                    settle_tool_task(result.expect("a running worker"), &task_indices, calls, &mut results);
                }
                _ = ticker.tick() => self.screen.tick_working(),
            }
        }
        if stopped {
            // Keep completed results even when Esc arrives before they were observed.
            while let Some(result) = workers.try_join_next() {
                settle_tool_task(result, &task_indices, calls, &mut results);
            }
            workers.abort_all();
            while let Some(result) = workers.join_next().await {
                settle_tool_task(result, &task_indices, calls, &mut results);
            }
        }
        (
            results
                .into_iter()
                .map(|result| result.expect("every call has a terminal result"))
                .collect(),
            stopped,
        )
    }

    /// The single overflow recovery attempt for this turn. Returns `Some(..)` when the turn
    /// was handled here, `None` when the caller should carry on with normal error handling.
    ///
    /// Nothing is announced here: the compaction this triggers puts the overflow banner in
    /// the footer, and it is already on screen before this returns.
    pub(super) async fn recover_overflow(
        &mut self,
        error_text: &str,
    ) -> anyhow::Result<Option<TurnEnd>> {
        let (_, model) = self.model()?;
        let is_overflow = error_text.is_empty() || compact::looks_like_overflow(error_text);
        if !is_overflow || !self.retry.available() || model.context_window.is_none() {
            return Ok(None);
        }
        self.retry.spend();
        let pruned = self.prune_context()?;
        let (_, model) = self.model()?;
        let limit = model
            .compaction_budget()
            .map_err(anyhow::Error::msg)?
            .threshold;
        if pruned && self.context_tokens(&self.session.context_messages(), &tools::specs()) <= limit
        {
            return Ok(Some(TurnEnd::Continue));
        }
        // The failed response was never appended; previous successful calls must survive.
        if let Err(err) = self.compact(Reason::Overflow, None).await {
            // The one thing left to say: the automatic recovery did not work, so what happens
            // next is the user's call.
            self.screen.push_lines(ui_compact::note_lines(
                &format!("压缩失败：{err}"),
                crate::ui::screen::Style::new(Color::Red),
            ));
            return Ok(Some(TurnEnd::Done));
        }
        Ok(Some(TurnEnd::Continue))
    }
}

/// Record the terminal outcome of one tool worker.
///
/// The call's own arguments are passed in so a call that never produced a result can still
/// say what was attempted. Without them the block would fall back to the bare tool name
/// here and to the command on the next resume, and the same interrupted call would read
/// differently in the two views.
fn settle_tool_task(
    result: Result<(usize, ToolOutput), tokio::task::JoinError>,
    task_indices: &std::collections::HashMap<tokio::task::Id, usize>,
    calls: &[(String, String, serde_json::Value)],
    results: &mut [Option<(ToolOutput, ToolStatus)>],
) {
    let (index, output, status) = match result {
        Ok((index, output)) => {
            let status = if output.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
            (index, output, status)
        }
        Err(error) => {
            let index = task_indices[&error.id()];
            let (name, arguments) = (&calls[index].1, &calls[index].2);
            // Both messages are written for the model, and both end up in its context: it
            // has to know the call did not complete and must check before retrying. The
            // transcript does not repeat them — see `ui::compact::result_text`.
            let (content, status) = if error.is_cancelled() {
                (
                    "用户中止了工具执行，可能已有部分效果；请先检查实际状态。".to_string(),
                    ToolStatus::Cancelled,
                )
            } else {
                (
                    format!("工具执行任务异常，结果未知：{error}"),
                    ToolStatus::Unknown,
                )
            };
            (
                index,
                ToolOutput::error_for(name, arguments, content),
                status,
            )
        }
    };
    results[index] = Some((output, status));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures build no thinking blocks, so both readings agree.
    const REPLAY: llm::ThinkingReplay = llm::ThinkingReplay::Dropped;

    fn test_agent() -> (Agent, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pi-boundaries-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":"http://127.0.0.1:9/v1","api_key":"local-test",
                "models":[{"id":"m","context_window":6000,"max_tokens":1000,"compaction":{"reserve_tokens":2000,"keep_recent_tokens":1000}}]}],
            "default_model":"test/m"
        })).unwrap();
        let mut agent = Agent::new(config, dir.clone(), false).unwrap();
        agent.client = Client::local_test_client();
        agent.session = Session::create_in(&dir, &dir, "test/m").unwrap();
        (agent, dir)
    }

    fn append_calls(agent: &mut Agent, calls: &[(String, String, serde_json::Value)]) {
        agent
            .session
            .push_message(
                Message::Assistant {
                    content: calls
                        .iter()
                        .map(|(id, name, arguments)| llm::Block::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        })
                        .collect(),
                    stop_reason: Some(StopReason::ToolUse),
                },
                None,
                None,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_and_crashed_workers_keep_completed_results_and_report_uncertainty() {
        let mut workers = tokio::task::JoinSet::new();
        let mut indices = std::collections::HashMap::new();
        let mut results = vec![None; 3];
        // The calls whose workers are being settled: a worker that never returned a result
        // still has to be describable in the transcript.
        let calls: Vec<(String, String, serde_json::Value)> = vec![
            (
                "a".into(),
                "bash".into(),
                serde_json::json!({"command": "ls -l"}),
            ),
            (
                "b".into(),
                "bash".into(),
                serde_json::json!({"command": "sleep 30"}),
            ),
            (
                "c".into(),
                "read".into(),
                serde_json::json!({"path": "x.rs"}),
            ),
        ];
        let completed = workers.spawn(async { (0, ToolOutput::text("completed result")) });
        indices.insert(completed.id(), 0);
        settle_tool_task(
            workers.join_next().await.unwrap(),
            &indices,
            &calls,
            &mut results,
        );

        let (started, running) = tokio::sync::oneshot::channel();
        let cancelled = workers.spawn(async {
            started.send(()).unwrap();
            std::future::pending::<(usize, ToolOutput)>().await
        });
        indices.insert(cancelled.id(), 1);
        running.await.unwrap();
        let crashed = workers.spawn(async { panic!("tool worker failed") });
        indices.insert(crashed.id(), 2);
        settle_tool_task(
            workers.join_next().await.unwrap(),
            &indices,
            &calls,
            &mut results,
        );
        workers.abort_all();
        while let Some(result) = workers.join_next().await {
            settle_tool_task(result, &indices, &calls, &mut results);
        }
        let results: Vec<_> = results.into_iter().map(Option::unwrap).collect();
        assert_eq!(results[0].0.content, "completed result");
        assert_eq!(results[0].1, ToolStatus::Success);
        assert_eq!(results[1].1, ToolStatus::Cancelled);
        assert!(results[1].0.content.contains("可能已有部分效果"));
        // The interrupted call still knows what it was running: without this echo the
        // block would say `⊘ bash` here and `⊘ $ sleep 30` on the next resume.
        assert!(matches!(
            results[1].0.display,
            crate::tools::Display::Command { .. }
        ));
        assert!(matches!(
            results[2].0.display,
            crate::tools::Display::File { .. }
        ));
        assert_eq!(results[2].1, ToolStatus::Unknown);
        assert!(results[2].0.content.contains("结果未知"));
    }

    #[tokio::test]
    async fn read_batches_preserve_result_order_and_mutation_boundaries() {
        let (mut agent, dir) = test_agent();
        std::fs::write(dir.join("shared"), "before").unwrap();
        std::fs::write(dir.join("other"), "other").unwrap();
        agent
            .session
            .push_message(Message::user_text("读取之后修改，再重新读取"), None, None)
            .unwrap();
        let calls = vec![
            (
                "0".into(),
                "read".into(),
                serde_json::json!({"path":"shared"}),
            ),
            (
                "1".into(),
                "read".into(),
                serde_json::json!({"path":"other"}),
            ),
            (
                "2".into(),
                "read".into(),
                serde_json::json!({"path":"missing"}),
            ),
            (
                "3".into(),
                "write".into(),
                serde_json::json!({"path":"shared","content":"after"}),
            ),
            (
                "4".into(),
                "read".into(),
                serde_json::json!({"path":"shared"}),
            ),
        ];
        append_calls(&mut agent, &calls);
        assert!(!agent.execute_tools(&calls).await.unwrap());
        let messages = agent.session.context_messages();
        llm::validate_tool_history(&messages).unwrap();
        let results: Vec<_> = messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool {
                    tool_call_id,
                    status,
                    content,
                    ..
                } => Some((tool_call_id.as_str(), *status, content.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            results.iter().map(|r| r.0).collect::<Vec<_>>(),
            ["0", "1", "2", "3", "4"]
        );
        assert!(results[0].2.contains("before"));
        assert_eq!(results[2].1, ToolStatus::Error);
        assert_eq!(results[3].1, ToolStatus::Success);
        assert!(results[4].2.contains("after"));
        assert_eq!(
            std::fs::read_to_string(dir.join("shared")).unwrap(),
            "after"
        );
        assert_eq!(agent.session.file_operations().lists().1, ["shared"]);
        drop(agent);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn steering_preserves_completed_work_and_skips_obsolete_calls() {
        use crate::ui::screen::Queued;
        let (mut agent, dir) = test_agent();
        std::fs::write(dir.join("original"), "original data").unwrap();
        agent
            .session
            .push_message(Message::user_text("先读取，再更新说明"), None, None)
            .unwrap();
        let done = vec![(
            "done".into(),
            "read".into(),
            serde_json::json!({"path":"original"}),
        )];
        append_calls(&mut agent, &done);
        agent.execute_tools(&done).await.unwrap();
        let obsolete = vec![(
            "obsolete".into(),
            "write".into(),
            serde_json::json!({"path":"must-not-exist","content":"old plan"}),
        )];
        append_calls(&mut agent, &obsolete);
        agent
            .screen
            .queue(Queued::Message("不用写文件，直接解释".into(), Vec::new()));
        assert!(!agent.execute_tools(&obsolete).await.unwrap());
        agent.accept_steering().unwrap();
        let messages = agent.session.context_messages();
        llm::validate_tool_history(&messages).unwrap();
        assert!(messages.iter().any(|m| matches!(m, Message::Tool { tool_call_id, status: ToolStatus::Success, .. } if tool_call_id == "done")));
        assert!(messages.iter().any(|m| matches!(m, Message::Tool { tool_call_id, status: ToolStatus::Skipped, .. } if tool_call_id == "obsolete")));
        assert_eq!(messages.first().unwrap().text(), "先读取，再更新说明");
        assert_eq!(messages.last().unwrap().text(), "不用写文件，直接解释");
        assert!(!dir.join("must-not-exist").exists());
        assert!(agent.session.file_operations().lists().1.is_empty());
        assert!(!agent.screen.has_steering());
        drop(agent);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn pressure_relief_skips_the_summary_request_and_resumes_durably() {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "request never arrived"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let event = serde_json::json!({"choices":[{"delta":{"content":"已分析"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1700,"completion_tokens":3}});
            let response = format!("data: {event}\n\ndata: [DONE]\n\n");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            request
        });
        let (mut agent, dir) = test_agent();
        agent.config.providers[0].base_url = format!("http://{address}/v1");
        agent
            .session
            .push_message(Message::user_text("分析输出，保留已有改动"), None, None)
            .unwrap();
        let calls = vec![(
            "large".into(),
            "read".into(),
            serde_json::json!({"path":"log"}),
        )];
        append_calls(&mut agent, &calls);
        let original = "x".repeat(40_000);
        agent
            .session
            .push_message(
                Message::Tool {
                    tool_call_id: "large".into(),
                    name: "read".into(),
                    content: original.clone(),
                    status: ToolStatus::Success,
                },
                None,
                None,
            )
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(10), agent.assistant_turn())
                .await
                .unwrap()
                .unwrap(),
            TurnEnd::Done
        ));
        let request = server.join().unwrap();
        assert_eq!(request["stream"], true);
        assert_ne!(
            request["tool_choice"], "none",
            "pressure was relieved without summarization"
        );
        assert!(request["messages"].to_string().contains("中间已裁剪"));
        assert_eq!(agent.session.totals.input, 1700);
        assert!(agent.session.records().iter().any(|r| matches!(
            r,
            crate::agent::session::Record::Pruned {
                tool_results: 1,
                ..
            }
        )));
        let path = agent.session.path().to_path_buf();
        let expected = agent.session.context_messages();
        drop(agent);
        let resumed = Session::open(&path).unwrap();
        assert_eq!(resumed.context_messages(), expected);
        assert_eq!(resumed.measured_context_tokens(REPLAY), Some(1703));
        assert!(
            resumed
                .records()
                .iter()
                .filter_map(crate::agent::session::Record::message)
                .any(|m| matches!(m, Message::Tool { content, .. } if content == &original))
        );
        drop(resumed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn permissions_is_a_no_op_without_a_terminal() {
        // The picker returns the *current* entry when there is no terminal, so a piped
        // `/permissions` cannot flip the switch: changing it takes a person clicking.
        let (mut agent, dir) = test_agent();
        assert!(!agent.screen.interactive());
        let before = agent.gate.mode();
        agent.command_permissions().unwrap();
        assert_eq!(agent.gate.mode(), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_permissions_menu_stacks_its_two_answers() {
        // What the command hands the renderer, checked without a terminal: the two answers
        // are separate entries and each carries its own second row. The unit test in
        // `ui::screen` proves the rows are shaped right; this proves `/permissions` is what
        // asks for that shape, which is the link a rendering test cannot see.
        use crate::auth::guard::PermissionMode;
        use crate::ui::screen::Choice;
        let choices: Vec<Choice> = [PermissionMode::Ask, PermissionMode::Allow]
            .into_iter()
            .map(|mode| Choice::with_detail(mode.label(), mode.detail()))
            .collect();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].label, "需要审核");
        assert_eq!(choices[1].label, "自动放行");
        assert_eq!(choices[0].detail.as_deref(), Some("执行前先问你一句"));
        assert_eq!(choices[1].detail.as_deref(), Some("直接执行，不再询问"));
        // Neither row restates the shared half: the two options run the same policy.
        for choice in &choices {
            let detail = choice.detail.clone().unwrap();
            assert!(!detail.contains("需要确认"), "{}", choice.label);
            assert!(!detail.contains(choice.label.as_str()), "{}", choice.label);
        }
    }

    #[test]
    fn turning_approval_off_is_recorded_and_survives_a_resume() {
        // The mode is how the session runs, so it belongs in the turn context next to the
        // model and level — a resume that came back as `Ask` would be a change nobody made.
        use crate::auth::guard::PermissionMode;
        let (mut agent, dir) = test_agent();
        agent.gate.set_mode(PermissionMode::Allow);
        agent
            .session
            .push_turn_context(&agent.cwd, "test/m", "high", PermissionMode::Allow)
            .unwrap();
        agent
            .session
            .push_message(Message::user_text("你好"), None, None)
            .unwrap();
        let path = agent.session.path().to_path_buf();
        drop(agent);

        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.current_permission_mode(), PermissionMode::Allow);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
