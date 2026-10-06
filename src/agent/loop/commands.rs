//! The slash commands: `/new`, `/resume`, `/model`, `/compact`, `/delete`.

use super::*;

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
        match name {
            "exit" | "quit" => return Ok(false),
            "model" => self.command_model().await?,
            "name" => self.command_name(argument)?,
            "compact" => self.command_compact(argument).await?,
            "new" => self.command_new()?,
            "resume" => self.command_resume().await?,
            "delete" => return self.command_delete(),
            other => {
                let available = COMMANDS
                    .iter()
                    .map(|(name, _)| format!("/{name}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("未知命令：/{other}（可用：{available}）"),
                    crate::ui::screen::Style::new(Color::Yellow),
                ));
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
            self.screen.push_lines(ui_compact::note_lines(
                "本会话没有内容，未生成文件。",
                crate::ui::screen::Style::new(Color::Dim),
            ));
            return Ok(false);
        }
        // A destructive action needs an explicit yes, and the non-interactive path has no
        // way to give one: `pick` answers with the highlighted entry there, and that would
        // turn `echo /delete | pi` into an unattended delete. Refuse instead, and say how
        // to do it deliberately.
        if !self.screen.interactive() {
            self.screen.push_lines(ui_compact::note_lines(
                &format!("未删除：需要交互确认。手动删除：rm {}", path),
                crate::ui::screen::Style::new(Color::Yellow),
            ));
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
        // The early return above proved there is a file, so the deletion is a fact and not a
        // question: report the path that is gone, which is the one thing the user cannot see
        // from the screen.
        let removed = self.session.delete()?;
        debug_assert!(removed, "a saved session always has a file to unlink");
        self.deleted = true;
        // The store's entry goes with the last session in it. Otherwise a directory stays
        // listed for every project that was ever used, and the store stops being a list of
        // where history *is* — which is the whole reason for grouping by directory.
        crate::config::forget_dir_if_empty(&self.cwd);
        self.screen.push_lines(ui_compact::note_lines(
            &format!("已删除会话文件：{path}"),
            crate::ui::screen::Style::new(Color::Dim),
        ));
        Ok(false)
    }

    pub(super) fn command_new(&mut self) -> anyhow::Result<()> {
        // The id is not announced: it is printed on the way out, with the command that
        // resumes it, which is the only place it is useful.
        self.start_new_session()
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

    pub(super) async fn command_resume(&mut self) -> anyhow::Result<()> {
        let summaries = crate::agent::session::list(&self.cwd);
        if summaries.is_empty() {
            self.screen.push_lines(ui_compact::note_lines(
                "还没有可恢复的会话。",
                crate::ui::screen::Style::new(Color::Dim),
            ));
            return Ok(());
        }
        let items: Vec<String> = summaries
            .iter()
            .map(|summary| {
                format!(
                    "{}   {}   {} 条消息",
                    summary.label(40),
                    summary.modified_label(),
                    summary.messages
                )
            })
            .collect();
        // Esc cancels, and cancelling a "go somewhere else" prompt leaves the user at a
        // blank line with no way to start over — the current session is still the one they
        // were trying to leave. So cancelling *is* the new session here. When the current
        // session has already said something it is kept (it stays in `/resume`), so nothing
        // is lost; the menu's own hint says what the key will do, and the wiped screen says
        // it happened, so no note follows.
        let Some(index) = self.screen.pick_with_hint(
            "恢复历史会话",
            "↑↓ 选择 · Enter 确认 · Esc 开始新会话",
            &items,
        ) else {
            self.start_new_session()?;
            return Ok(());
        };
        let target = summaries[index].path.clone();
        if target == self.session.path() {
            return Ok(());
        }
        match Session::open(&target) {
            Ok(session) => {
                self.restore_session(session)?;
            }
            Err(err) => {
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("无法打开会话：{err}"),
                    crate::ui::screen::Style::new(Color::Red),
                ));
            }
        }
        Ok(())
    }

    /// `/model`: pick a model, then — only for a reasoning model — pick a thinking level.
    ///
    /// The two-step flow is why there is no `/thinking` command: the level belongs to the
    /// model, and a model that cannot reason is asked nothing.
    pub(super) async fn command_model(&mut self) -> anyhow::Result<()> {
        let catalogue = self.config.catalogue();
        if catalogue.is_empty() {
            self.screen.push_lines(ui_compact::note_lines(
                "配置里没有模型。",
                crate::ui::screen::Style::new(Color::Yellow),
            ));
            return Ok(());
        }
        let current_index = catalogue
            .iter()
            .position(|(_, spec)| *spec == self.model_spec)
            .unwrap_or(0);
        let items: Vec<String> = catalogue
            .iter()
            .map(|(provider, spec)| {
                let (_, model) = self.config.find(spec).unwrap();
                // The listed entries and their levels are exactly what the config declares;
                // nothing is hidden and nothing is added.
                if model.reasoning {
                    format!(
                        "{} ({} · {})",
                        model.display_name(),
                        provider,
                        model.levels().join("/")
                    )
                } else {
                    format!("{} ({})", model.display_name(), provider)
                }
            })
            .collect();
        let Some(index) = self.screen.pick_at("选择模型", &items, current_index) else {
            return Ok(());
        };
        let spec = catalogue[index].1.clone();
        self.model_spec = spec.clone();
        let (_, model) = self.config.find(&spec).expect("chosen from the catalogue");
        let levels = model.levels();
        // The footer is redrawn with every change, so the model and level that are now in
        // force need no note. The one thing it cannot show is a level the user asked for and
        // did not get, because that was silently replaced.
        let mut clamped_from: Option<String> = None;

        if levels.is_empty() {
            self.level.clear();
        } else {
            let current = levels
                .iter()
                .position(|level| *level == self.level)
                .unwrap_or(0);
            let level_items: Vec<String> = levels.iter().map(|level| level.to_string()).collect();
            if let Some(chosen) = self.screen.pick_at("思考级别", &level_items, current) {
                self.level = levels[chosen].clone();
            }
            // A level that came from another model may not exist here.
            let (clamped, moved) = llm::clamp_level(model, &self.level);
            if moved {
                clamped_from = Some(self.level.clone());
            }
            self.level = clamped;
        }
        if let Some(asked) = clamped_from {
            self.screen.push_lines(ui_compact::note_lines(
                &format!("思考级别 {asked} 不受支持，已调整为 {}", self.level),
                crate::ui::screen::Style::new(Color::Dim),
            ));
        }
        self.session
            .push_turn_context(&self.cwd, &self.model_spec, &self.level)?;
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
                self.screen.push_lines(ui_compact::note_lines(
                    &err.to_string(),
                    crate::ui::screen::Style::new(Color::Yellow),
                ));
            }
        };
        Ok(())
    }

    /// Run one compaction and write the checkpoint.
    ///
    /// Every refusal lives here rather than at the call sites: the check that a compaction
    /// cannot start while an answer is streaming used to be spelled out in `command_compact`
    /// as well, and the two could drift apart.
    pub(super) async fn compact(
        &mut self,
        reason: Reason,
        custom: Option<&str>,
    ) -> Result<(), CompactError> {
        if self.streaming {
            return Err(CompactError::Streaming);
        }
        self.compaction.begin()?;
        self.render_footer(Some(reason.banner()));
        let result = self.compact_inner(reason, custom).await;
        self.compaction.finish();
        self.render_footer(None);
        result
    }

    pub(super) async fn compact_inner(
        &mut self,
        reason: Reason,
        custom: Option<&str>,
    ) -> Result<(), CompactError> {
        let (provider, model) = {
            let (provider, model) = self
                .model()
                .map_err(|err| CompactError::Summarize(err.to_string()))?;
            (provider.clone(), model.clone())
        };
        let snapshot = self.session.context_snapshot();
        let messages = snapshot.messages;
        let context_ids = snapshot.entry_ids;
        let tools = tools::specs();
        let summary_session_id = format!("summary-{}", uuid::Uuid::now_v7());
        let request = SummaryRequest {
            provider: &provider,
            model: &model,
            session_id: &summary_session_id,
            system_prompt: self.system_prompt.as_deref(),
            tools: &tools,
            level: &self.level,
            custom_instructions: custom,
        };
        let budget = model.compaction_budget().map_err(CompactError::Summarize)?;
        let facts = self.session.checkpoint_facts(
            (model.context_window.unwrap_or(128_000) / 12).min(1024) as usize * 4,
        );
        let compaction_id = uuid::Uuid::now_v7().to_string();
        let token_before = llm::estimate_request_context(
            &messages,
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
        let outcome = compact::run(
            &self.client,
            request,
            &messages,
            &context_ids,
            &compaction_id,
            budget.keep_recent,
            facts,
        )
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => {
                self.session
                    .push_compaction_end(&compaction_id, Some(&err.to_string()))
                    .map_err(|session_err| CompactError::Session(session_err.to_string()))?;
                return Err(err);
            }
        };
        self.session
            .push_compaction(
                outcome,
                reason.label(),
                &provider.name,
                &model.id,
                &self.level,
            )
            .map_err(|err| CompactError::Session(err.to_string()))?;
        self.session
            .push_compaction_end(&compaction_id, None)
            .map_err(|err| CompactError::Session(err.to_string()))?;
        // The token counts are not restated: the footer carries the context gauge, and that is
        // the number the user is already reading. A compaction that shrank nothing is not a
        // quietly-worded success either — it fails as `NotSmaller` and never reaches here.
        let note = "已压缩";
        self.screen.push_lines(ui_compact::note_lines(
            note,
            crate::ui::screen::Style::new(Color::Dim),
        ));
        Ok(())
    }
}
