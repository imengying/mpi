//! The slash commands: `/new`, `/resume`, `/model`, `/compact`, `/delete`.

use super::*;
use crate::ui::screen::Choice;

pub(super) enum SelectionKind {
    Model(Vec<String>),
    Level { spec: String, levels: Vec<String> },
    Permissions(Vec<crate::auth::guard::PermissionMode>),
    Resume(Vec<PathBuf>),
}

impl Agent {
    /// Handle a slash command. Returns false when the session should end.
    pub async fn command(&mut self, input: &str) -> anyhow::Result<bool> {
        let (name, argument) = match input.strip_prefix('/') {
            Some(rest) => match rest.split_once(' ') {
                Some((name, argument)) => (name, argument.trim()),
                None => (rest.trim(), ""),
            },
            None => return Ok(true),
        };
        self.screen.close_views();
        if self.running && matches!(name, "compact" | "new" | "resume" | "delete") {
            self.screen.push_lines(ui_compact::warning(&format!(
                "/{name}：请先按 Esc 停止任务"
            )));
            self.screen.flush();
            return Ok(true);
        }
        match name {
            "exit" | "quit" => {
                self.exit_requested = true;
                return Ok(false);
            }
            "model" => self.command_model()?,
            "name" => self.command_name(argument)?,
            "permissions" => self.command_permissions()?,
            "compact" => self.command_compact(argument).await?,
            "new" => self.start_new_session()?,
            "resume" => self.command_resume()?,
            "delete" => return self.command_delete(),
            other => {
                self.screen.push_lines(ui_compact::warning(&format!(
                    "未知命令：/{other}。输入 / 查看命令"
                )));
            }
        }
        Ok(true)
    }

    pub(super) fn command_name(&mut self, argument: &str) -> anyhow::Result<()> {
        // `/name` without an argument clears the name, exactly like pi.
        let name = if argument.is_empty() {
            None
        } else {
            Some(argument.replace('\n', " "))
        };
        self.session.set_name(name.as_deref())?;
        // No confirmation line: the name is drawn in the footer on the very next frame, so a
        // note repeating it here would be the second copy of the same fact.
        Ok(())
    }

    /// `/delete`: remove this session's file and leave.
    ///
    /// It asks first. The file is the only record of the conversation, and unlike every
    /// other command here there is nothing to recover from — `/resume` will simply no
    /// longer list it.
    ///
    /// Returns `false` to end the turn loop, which is the point of the command: the session
    /// is gone, so there is nothing left to continue.
    pub(super) fn command_delete(&mut self) -> anyhow::Result<bool> {
        let name = self
            .session
            .name()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "未命名".into());
        let path = self.session.path().display().to_string();
        // Nothing was written, so there is nothing to confirm or to remove. This check
        // comes first because it is the accurate answer: the other branches would offer to
        // `rm` a path that does not exist.
        if !self.session.is_saved() {
            self.deleted = true;
            self.screen.push_lines(ui_compact::note("会话为空"));
            return Ok(false);
        }
        // A destructive action needs an explicit yes, and the non-interactive path has no
        // way to give one: `pick` answers with the highlighted entry there, and that would
        // turn `echo /delete | pi` into an unattended delete. Refuse instead, and say how
        // to do it deliberately.
        if !self.screen.interactive() {
            self.screen.push_lines(ui_compact::warning(&format!(
                "未删除（需要交互确认）。rm {}",
                path
            )));
            return Ok(true);
        }
        // A refusal is still reported: the session ends only when the user says so, and the
        // manual command is the one thing they can do about it.
        let choice = self.screen.pick(
            &format!("删除会话「{name}」？"),
            &["不删，继续".to_string(), "是的，删除并退出".to_string()],
        );
        // The safe option is highlighted first, so a reflexive Enter keeps the session. No
        // note for it: the session is still on screen, which is the answer to "was it kept?".
        if choice != Some(1) {
            return Ok(true);
        }
        // The early return above proved there is a file; confirm removal before leaving.
        let removed = self.session.delete()?;
        debug_assert!(removed, "a saved session always has a file to unlink");
        self.deleted = true;
        // The store's entry goes with the last session in it. Otherwise a directory stays
        // listed for every project that was ever used, and the store stops being a list of
        // where history *is* — which is the whole reason for grouping by directory.
        crate::agent::session::dirs::forget_dir_if_empty(&self.cwd);
        self.screen.push_lines(ui_compact::note("已删除会话"));
        Ok(false)
    }

    /// Replace the current session with a fresh one, keeping the working directory.
    ///
    /// The old session is not deleted: it is on disk and still reachable through `/resume`,
    /// so "new" costs nothing and undo is a resume away.
    ///
    /// Nothing is printed: the transcript is wiped, and an empty screen under a footer with
    /// no session name already says "new session" more plainly than a line of prose would.
    pub(super) fn start_new_session(&mut self) -> anyhow::Result<()> {
        let model_spec = self.model_spec.clone();
        self.session = Session::create(&self.cwd, &model_spec)?;
        self.gate.clear_approvals();
        self.observed = tools::observed::Observed::default();
        // Nothing has been timed in the new session yet.
        self.output_rate = None;
        self.system_prompt = system_prompt_from(&self.cwd);
        let block = environment_block(
            &self.cwd,
            &self.session.header().id,
            &self.config.shell.path,
        );
        self.session
            .push_message(Message::user_text(block), None, None)?;
        self.screen.clear_transcript();
        // A new session starts with no history: the lines from the conversation just left
        // belong to it, not to this one, and offering them here would be recalling words
        // this session never heard.
        self.screen.seed_history(Vec::new());
        Ok(())
    }

    /// `/permissions`: choose whether the policy's questions are asked.
    ///
    /// Two answers, stacked the way the authorization panel stacks its own: one entry per
    /// block, the selected one painted across the width. The current answer starts
    /// highlighted, so Enter keeps it.
    ///
    /// Only the *asking* is decided here. Nothing the policy already allows is affected.
    pub(super) fn command_permissions(&mut self) -> anyhow::Result<()> {
        use crate::auth::guard::PermissionMode;
        let modes = vec![PermissionMode::Ask, PermissionMode::Allow];
        let current = modes
            .iter()
            .position(|mode| *mode == self.gate.mode())
            .unwrap_or(0);
        let choices = modes
            .iter()
            .map(|mode| Choice::with_detail(mode.label(), mode.detail()))
            .collect();
        self.choose(
            SelectionKind::Permissions(modes),
            "命令审核",
            choices,
            current,
        )
    }

    pub(super) fn command_resume(&mut self) -> anyhow::Result<()> {
        let summaries = crate::agent::session::list(&self.cwd);
        if summaries.is_empty() {
            self.screen.push_lines(ui_compact::note("暂无历史会话"));
            return Ok(());
        }
        let choices = summaries
            .iter()
            .map(|summary| {
                Choice::new(format!(
                    "{}   {}   {} 条消息",
                    summary.label(40),
                    summary.modified_label(),
                    summary.messages,
                ))
            })
            .collect();
        self.choose(
            SelectionKind::Resume(summaries.into_iter().map(|summary| summary.path).collect()),
            "恢复历史会话",
            choices,
            0,
        )
    }

    pub(super) fn command_model(&mut self) -> anyhow::Result<()> {
        let catalogue = self.config.catalogue();
        if catalogue.is_empty() {
            self.screen
                .push_lines(ui_compact::warning("配置里没有模型。"));
            return Ok(());
        }
        let current = catalogue
            .iter()
            .position(|(_, spec)| *spec == self.model_spec)
            .unwrap_or(0);
        let choices = catalogue
            .iter()
            .map(|(provider, spec)| {
                let (_, model) = self.config.find(spec).unwrap();
                Choice::new(if model.reasoning {
                    format!(
                        "{} ({} · {})",
                        model.display_name(),
                        provider,
                        model.levels().join("/")
                    )
                } else {
                    format!("{} ({})", model.display_name(), provider)
                })
            })
            .collect();
        self.choose(
            SelectionKind::Model(catalogue.into_iter().map(|(_, spec)| spec).collect()),
            "选择模型",
            choices,
            current,
        )
    }

    fn choose(
        &mut self,
        kind: SelectionKind,
        title: &str,
        choices: Vec<Choice>,
        initial: usize,
    ) -> anyhow::Result<()> {
        self.selection = Some(kind);
        if self.screen.interactive() {
            self.screen
                .open_picker(title, "↑↓ 选择 · Enter 确认 · Esc 取消", choices, initial);
            Ok(())
        } else {
            self.handle_selection(Some(initial))
        }
    }

    /// Apply picker selections without pausing the active stream or tool batch.
    pub fn handle_selection(&mut self, selected: Option<usize>) -> anyhow::Result<()> {
        let Some(kind) = self.selection.take() else {
            return Ok(());
        };
        let Some(index) = selected else { return Ok(()) };
        match kind {
            SelectionKind::Model(specs) => {
                let Some(spec) = specs.get(index) else {
                    return Ok(());
                };
                let (_, model) = self.config.find(spec).expect("listed model");
                let levels = model.levels();
                if levels.is_empty() {
                    self.select_model(spec.clone(), String::new())?;
                } else {
                    let current = levels
                        .iter()
                        .position(|level| *level == self.level)
                        .unwrap_or(0);
                    let choices = levels.iter().map(Choice::new).collect();
                    self.choose(
                        SelectionKind::Level {
                            spec: spec.clone(),
                            levels,
                        },
                        "思考级别",
                        choices,
                        current,
                    )?;
                }
            }
            SelectionKind::Level { spec, levels } => {
                if let Some(level) = levels.get(index) {
                    self.select_model(spec, level.clone())?;
                }
            }
            SelectionKind::Permissions(modes) => {
                if let Some(mode) = modes.get(index)
                    && *mode != self.gate.mode()
                {
                    self.gate.set_mode(*mode);
                    self.session.push_turn_context(
                        &self.cwd,
                        &self.model_spec,
                        &self.level,
                        *mode,
                    )?;
                }
            }
            SelectionKind::Resume(paths) => {
                let Some(path) = paths.get(index).filter(|path| *path != self.session.path())
                else {
                    return Ok(());
                };
                match Session::open(path) {
                    Ok(session) => self.restore_session(session)?,
                    Err(err) => self
                        .screen
                        .push_lines(ui_compact::error(&format!("无法打开会话：{err}"))),
                }
            }
        }
        self.render_footer(None);
        Ok(())
    }

    fn select_model(&mut self, spec: String, level: String) -> anyhow::Result<()> {
        self.model_spec = spec;
        self.level = level;
        self.output_rate = None;
        self.session.push_turn_context(
            &self.cwd,
            &self.model_spec,
            &self.level,
            self.gate.mode(),
        )?;
        Ok(())
    }

    /// `/compact`: always allowed on request, but it reports the reason when there is
    /// nothing to cut.
    pub(super) async fn command_compact(&mut self, instructions: &str) -> anyhow::Result<()> {
        let custom = (!instructions.trim().is_empty()).then_some(instructions);
        // The footer's banner is the whole indication here. A spinner would be the second
        // one, and a spinner that nothing ticks is worse than none: it sits at its first
        // frame for as long as the summary takes, which is the look of a hung process.
        match self.compact(Reason::Manual, custom).await {
            Ok(()) => {}
            Err(err) => {
                self.screen
                    .push_lines(ui_compact::warning(&err.to_string()));
            }
        };
        Ok(())
    }

    /// Run one compaction and write the checkpoint.
    ///
    /// Every refusal lives here rather than at the call sites: the check that a compaction
    /// cannot start while an answer is streaming used to be spelled out in `command_compact`
    /// as well, and the two could drift apart.
    ///
    /// The summary request is the longest single `await` in the program — a whole session
    /// summarised by a model — and the composer has to stay alive through it or the screen
    /// simply freezes for however long it takes. That is why the work is split in three:
    /// prepare (borrows the agent), run (owns everything it touches, so it can sit in a
    /// `select!` next to input), and finish (borrows the agent again). Only the middle part
    /// is cancellable, and cancelling it leaves a durable record of what was attempted.
    pub(super) async fn compact(
        &mut self,
        reason: Reason,
        custom: Option<&str>,
    ) -> Result<(), CompactError> {
        if self.streaming {
            return Err(CompactError::Streaming);
        }
        self.compaction.begin()?;
        let result = self.compact_watched(reason, custom).await;
        self.compaction.finish();
        self.render_footer(None);
        result
    }

    /// The cancellable half of [`Self::compact`]: keeps the spinner turning and the composer
    /// live while the summary is generated.
    async fn compact_watched(
        &mut self,
        reason: Reason,
        custom: Option<&str>,
    ) -> Result<(), CompactError> {
        let job = self.prepare_compaction(reason, custom)?;
        self.render_footer(Some(reason.banner()));
        self.screen.set_working(WORKING_LABEL);
        // The banner is what the user is waiting on, so it has to be on screen before the
        // first await — `render()` is coalesced and would otherwise drop it.
        self.screen.flush();
        let running = job.run(self.client.clone());
        tokio::pin!(running);
        let mut ticker = ticker();
        let mut dropped_command = None;
        let outcome = loop {
            // Esc is the same key that stops a turn; here it abandons the summary. Text typed
            // meanwhile is queued exactly as it is during a turn — but a slash command is not
            // dispatched here: `/compact` reaching this loop would start a second summary on
            // top of this one, so commands wait for the compaction to finish.
            if let Some(action) = self.screen.poll_input() {
                match action {
                    Action::Stop | Action::Interrupt => break Err(CompactError::Cancelled),
                    Action::Line(text) => {
                        if queue_mid_turn(&mut self.screen, text, Vec::new()).is_some() {
                            dropped_command =
                                Some("压缩期间命令不执行；完成后请重新输入".to_string());
                        }
                    }
                    Action::LineWithImages(text, images) => {
                        if queue_mid_turn(&mut self.screen, text, images).is_some() {
                            dropped_command =
                                Some("压缩期间命令不执行；完成后请重新输入".to_string());
                        }
                    }
                    action => {
                        // Tool details and picker picks do not dispatch a command, so they are
                        // safe to handle inline. Only the `Action::Line*` arms above could
                        // reach `command()`, and those are handled without it.
                        match action {
                            Action::ToolDetails => {
                                self.screen.show_tool_details();
                            }
                            Action::Selection(index) => {
                                if let Err(err) = self.handle_selection(index) {
                                    self.screen.push_lines(ui_compact::error(&err.to_string()));
                                }
                            }
                            _ => {}
                        }
                        self.render_footer(None);
                    }
                }
            }
            tokio::select! {
                biased;
                outcome = &mut running => break outcome,
                _ = ticker.tick() => self.screen.tick_working(),
            }
        };
        if let Some(note) = dropped_command {
            self.screen.push_lines(ui_compact::warning(&note));
        }
        self.screen.clear_working();
        // A cancelled summary never produced a checkpoint, and an interrupted request may have
        // been on the wire: both leave the start event without a matching end for a reason the
        // file should state, rather than looking like a crash on resume.
        match outcome {
            Ok(outcome) => self.finish_compaction(&job, outcome),
            Err(err) => {
                let _ = self
                    .session
                    .push_compaction_end(&job.compaction_id, Some(&err.to_string()));
                Err(err)
            }
        }
    }

    fn prepare_compaction(
        &mut self,
        reason: Reason,
        custom: Option<&str>,
    ) -> Result<CompactionJob, CompactError> {
        let (provider, model) = {
            let (provider, model) = self
                .model()
                .map_err(|err| CompactError::Summarize(err.to_string()))?;
            (provider.clone(), model.clone())
        };
        let snapshot = self.session.context_snapshot();
        let tools = tools::specs();
        let budget = model.compaction_budget().map_err(CompactError::Summarize)?;
        let facts = self.session.checkpoint_facts(
            (model.context_window.unwrap_or(128_000) / 12).min(1024) as usize * 4,
        );
        let compaction_id = uuid::Uuid::now_v7().to_string();
        let token_before = llm::estimate_request_context(
            &snapshot.messages,
            self.system_prompt.as_deref().unwrap_or_default(),
            &tools,
        );
        self.session
            .push_compaction_start(
                &compaction_id,
                reason.label(),
                &provider.name,
                &model.id,
                &self.level,
                token_before,
            )
            .map_err(|err| CompactError::Session(err.to_string()))?;
        Ok(CompactionJob {
            reason,
            compaction_id,
            request: OwnedSummaryRequest {
                provider,
                model,
                session_id: format!("summary-{}", uuid::Uuid::now_v7()),
                system_prompt: self.system_prompt.clone(),
                tools,
                level: self.level.clone(),
                custom_instructions: custom.map(str::to_string),
            },
            keep_recent: budget.keep_recent,
            messages: snapshot.messages,
            context_ids: snapshot.entry_ids,
            facts,
        })
    }

    /// Write the checkpoint a finished job produced.
    fn finish_compaction(
        &mut self,
        job: &CompactionJob,
        outcome: compact::CompactionOutcome,
    ) -> Result<(), CompactError> {
        let (provider, model) = (&job.request.provider.name, &job.request.model.id);
        self.session
            .push_compaction(
                outcome,
                job.reason.label(),
                provider,
                model,
                &job.request.level,
            )
            .map_err(|err| CompactError::Session(err.to_string()))?;
        self.session
            .push_compaction_end(&job.compaction_id, None)
            .map_err(|err| CompactError::Session(err.to_string()))?;
        // The token counts are not restated: the footer carries the context gauge, and that is
        // the number the user is already reading. A compaction that shrank nothing is not a
        // quietly-worded success either — it fails as `NotSmaller` and never reaches here.
        if job.reason == Reason::Manual {
            self.screen.push_lines(ui_compact::note("已压缩"));
        }
        Ok(())
    }
}

/// One prepared compaction, owning everything its model call needs.
///
/// It exists as a type so the long `await` can be driven from a `select!` that also reads
/// input: nothing in here borrows the agent or the session.
pub(super) struct CompactionJob {
    reason: Reason,
    compaction_id: String,
    request: OwnedSummaryRequest,
    keep_recent: u64,
    messages: Vec<Message>,
    context_ids: Vec<String>,
    facts: CheckpointFacts,
}

impl CompactionJob {
    /// Run the summary. Borrows the job rather than consuming it, so the caller can still
    /// write the checkpoint (and record a failure) from the same values afterwards.
    async fn run(&self, client: Client) -> Result<compact::CompactionOutcome, CompactError> {
        compact::run(
            &client,
            self.request.request(),
            &self.messages,
            &self.context_ids,
            &self.compaction_id,
            self.keep_recent,
            self.facts.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary server that holds the request open, so the compaction is provably still in
    /// flight while the test looks at the screen.
    fn slow_summary_server(
        delay: std::time::Duration,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{BufRead, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = arrived.clone();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            seen.store(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(delay);
            let response = serde_json::json!({
                "status":"completed",
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"## Goal\n测试"}]}],
                "usage":{"input_tokens":100,"output_tokens":10}})
            .to_string();
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            );
        });
        (format!("http://{address}/v1"), handle, arrived)
    }

    fn compacting_agent(base_url: &str) -> (Agent, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pi-compact-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"responses","base_url":base_url,"api_key":"local-test",
                "models":[{"id":"m","context_window":6000,"max_tokens":1000,
                           "compaction":{"reserve_tokens":2000,"keep_recent_tokens":1000}}]}],
            "default_model":"test/m"
        }))
        .unwrap();
        let mut agent = Agent::new(config, dir.clone(), false).unwrap();
        agent.client = Client::local_test_client();
        agent.session = Session::create_in(&dir, &dir, "test/m").unwrap();
        for turn in 0..20 {
            agent
                .session
                .push_message(Message::user_text(format!("任务 {turn}")), None, None)
                .unwrap();
            agent
                .session
                .push_message(
                    Message::assistant_text(format!("回答 {turn}{}", "细节".repeat(200))),
                    None,
                    None,
                )
                .unwrap();
        }
        (agent, dir)
    }

    /// The bug this guards: `/compact` awaited the summary with nobody pumping events, so the
    /// banner never reached the screen and nothing moved for the whole request.
    ///
    /// What matters is *when* the banner arrives: it has to be written while the request is
    /// still outstanding, not after it returns.
    #[tokio::test]
    async fn a_compaction_paints_its_banner_before_waiting_on_the_summary() {
        let (base_url, server, arrived) =
            slow_summary_server(std::time::Duration::from_millis(500));
        let (mut agent, dir) = compacting_agent(&base_url);
        let output = crate::ui::screen::tests::CapturedOutput::default();
        agent.screen.set_output(Box::new(output.clone()));
        // The live frame is only painted in interactive mode; a non-interactive screen
        // streams the transcript to the same writer instead, and would not exercise this.
        agent.screen.pretend_interactive();

        let mut banner_seen_early = false;
        {
            let mut compaction = std::pin::pin!(agent.compact(Reason::Manual, None));
            let mut started = false;
            loop {
                tokio::select! {
                    result = &mut compaction => {
                        assert!(result.is_ok(), "{result:?}");
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {
                        if !started && arrived.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                            started = true;
                            banner_seen_early = output.text().contains("正在压缩");
                        }
                    }
                }
            }
        }
        assert!(
            banner_seen_early,
            "the banner has to be painted before the summary is awaited, not after it returns"
        );
        let _ = server.join();
        let _ = std::fs::remove_dir_all(dir);
    }
}
