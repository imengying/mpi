//! Running the tools the model asked for, and what to do when they hurt.

use super::*;

impl Agent {

        /// Execute every tool call from one assistant message, in order.
        ///
        /// A refused call still produces a result: the model receives the refusal text with
        /// the policy's reason, and the transcript shows the same failure. Skipping the tool
        /// result entirely would break the call/result pairing the providers expect.
        ///
        /// Returns `true` when Esc asked for the turn to stop; the calls already run keep their
        /// results, because the model has to see them as the results of the calls it made.
        pub(super) fn execute_tools(&mut self, calls: &[(String, String, serde_json::Value)]) -> bool {
            let cwd = self.cwd.clone();
            // Where the calls that never ran start, once Esc has stopped one of them.
            let mut skipped_from = None;
            for (index, (id, name, arguments)) in calls.iter().enumerate() {
                // The call is shown while it runs: a command can take minutes, and the screen
                // would otherwise sit unchanged with no sign that anything is happening. The
                // line is taken down just before the finished call is committed, so only one of
                // the two is ever on screen.
                let running = ui_compact::running_line(name, arguments);
                self.render_footer(None);
                self.screen.set_running(running.clone());
                // The authorization panel writes to the terminal directly and moves the cursor
                // itself, so the live region is taken down first and put back afterwards.
                // Otherwise the panel leaves the running line stranded above the call it
                // belongs to.
                self.screen.suspend_live();
                let output = match block_on(self.gate.check(id, name, arguments, &cwd)) {
                    Ok(()) => {
                        // Approved: the wait is now the command's own, so the running line goes
                        // back up before it starts, and the spinner keeps turning for as long as
                        // it takes.
                        self.screen.set_running(running);
                        let (result, stopped) = self.run_tool_until_stopped(name, arguments);
                        self.gate.finish(id);
                        if stopped {
                            skipped_from = Some(index + 1);
                        }
                        result
                    }
                    Err(refusal) => {
                        self.gate.finish(id);
                        // The refusal is echoed with the attempted call, so the transcript shows
                        // what the user declined rather than an anonymous failure.
                        ToolOutput::error_for(name, arguments, refusal.message())
                    }
                };
                self.screen.clear_running();
                let _ = self.session.push_message(
                    Message::Tool {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        content: output.content.clone(),
                    },
                    None,
                    None,
                );
                let block = ui_compact::tool_block(name, arguments, &output);
                self.screen.push(block);
                // The stopped call's own result is recorded above, like any other; only the
                // calls after it are skipped.
                if skipped_from.is_some() {
                    break;
                }
            }
            let Some(next) = skipped_from else { return false };
            // A call that never ran must still be answered: a tool call with no result makes the
            // next request invalid, and every provider rejects it. "The user stopped the turn" is
            // the honest reason to hand the model.
            for (id, name, _) in &calls[next..] {
                let _ = self.session.push_message(
                    Message::Tool {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        content: "[用户停止了本轮，这个调用没有执行]".to_string(),
                    },
                    None,
                    None,
                );
            }
            true
        }


        /// Run one approved tool call, watching for Esc.
        ///
        /// The command's process is killed when Esc arrives — `kill_on_drop` on the child does
        /// it, because the future is dropped and with it the child — so a `sleep 300` does not
        /// keep the turn alive after the user asked it to stop.
        pub(super) fn run_tool_until_stopped(
            &mut self,
            name: &str,
            arguments: &serde_json::Value,
        ) -> (ToolOutput, bool) {
            let cwd = self.cwd.clone();
            block_on(async {
                let work = tools::execute(name, arguments, &cwd);
                tokio::pin!(work);
                let mut ticker = ticker();
                let mut stop_requested = false;
                let mut result = None;
                loop {
                    if let Some(action) = self.screen.poll_input()
                        && on_turn_action(&mut self.screen, action)
                    {
                        stop_requested = true;
                    }
                    if stop_requested {
                        break;
                    }
                    tokio::select! {
                        out = &mut work => {
                            result = Some(out);
                            break;
                        }
                        _ = ticker.tick() => self.screen.tick_working(),
                    }
                }
                match result {
                    Some(output) => (output, false),
                    // The process is killed as the future is dropped here.
                    None => (
                        ToolOutput::error("[用户停止了本轮，命令已被终止]").timed(std::time::Duration::ZERO),
                        true,
                    ),
                }
            })
        }


        /// The single overflow recovery attempt for this turn. Returns `Some(..)` when the turn
        /// was handled here, `None` when the caller should carry on with normal error handling.
        ///
        /// Nothing is announced here: the compaction this triggers puts the overflow banner in
        /// the footer, and it is already on screen before this returns.
        pub(super) async fn recover_overflow(&mut self, error_text: &str) -> anyhow::Result<Option<TurnEnd>> {
            let (_, model) = self.model()?;
            let is_overflow = error_text.is_empty()
                || compact::looks_like_overflow(error_text);
            if !is_overflow || !self.retry.available() || model.context_window.is_none() {
                return Ok(None);
            }
            self.retry.spend();
            // Drop the failed assistant message before compacting: keeping it would fold a
            // half-written answer into the summary.
            self.session.drop_last_assistant()?;
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
