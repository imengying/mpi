//! The agent loop: one user turn in, streamed assistant output and tool calls out.
//!
//! Two invariants shape this file:
//!
//! * **The system prompt never changes.** It contains no cwd, no clock, no branch and no
//!   model name, so it stays a stable cache prefix for the whole session. Everything that
//!   describes the environment lives in a single session-stable block at the head of the
//!   conversation (see [`environment_block`]).
//! * **pi ships no prompt of its own.** The system message is built from `AGENTS.md` and
//!   nothing else, so a session without that file sends no system message at all. The
//!   prompt is therefore the user's, and it is theirs to change at any time — it is read
//!   once per session and travels with the session file.
//! * **Every risky action goes through the gate.** Tool calls are assessed one at a time;
//!   a refusal is handed back to the model as text with the reason attached, because a
//!   refusal without a reason makes the model retry the same command forever.

use std::path::{Path, PathBuf};

use crate::agent::compact::{self, CompactError, CompactionState, Reason, RetryBudget, SummaryRequest};
use crate::agent::session::Session;
use crate::auth::guard::PermissionGate;
use crate::auth::policy::{self, Dialect};
use crate::config::{Config, Defaults, ModelConfig, Provider};
use crate::llm::client::Client;
use crate::llm::{self, Delta, Message, Request, StopReason};
use crate::tools::{self, ToolOutput};
use crate::ui::compact as ui_compact;
use crate::ui::footer::{self, FooterState};
use crate::ui::screen::{Action, Screen, WORKING_INTERVAL, WORKING_LABEL};
use crate::ui::theme::Color;
use crate::util;

/// The slash commands pi accepts, with the one-line description the menu shows.
///
/// One list, used three ways: the dispatcher matches against it, the input area completes
/// against it, and the menu prints it. A command cannot be added to one and forgotten in the
/// others.
pub const COMMANDS: &[(&str, &str)] = &[
    ("model", "选择模型（支持推理的会接着问思考级别）"),
    ("name", "设置会话名；不带参数则清空"),
    ("compact", "手动压缩上下文，可带一段自定义指示"),
    ("new", "新会话"),
    ("resume", "恢复历史会话"),
    ("delete", "删除本会话并退出"),
    ("exit", "退出"),
];

/// The system message for a session, built from `AGENTS.md` and nothing else.
///
/// pi deliberately ships no prompt of its own: the instructions a model follows are the
/// user's, written down in a file they can edit and version. When no such file exists the
/// return value is `None` and no system message is sent — an empty one would be a wasted
/// cache entry and a lie about where the instructions came from.
///
/// The value is computed once per session and kept in [`Agent`], so it is byte-identical on
/// every request of that session: a stable cache prefix, exactly like the constant it
/// replaces.
fn system_prompt_from(cwd: &Path) -> Option<String> {
    load_agents_md(cwd).map(|(_, text)| text)
}

/// A per-session environment snapshot. It is stored as the head of the conversation rather
/// than in the system prompt, and it does not change while the session lives — a mutable
/// clock in here would break the cache on every turn.
pub fn environment_block(cwd: &Path, session_id: &str, shell: &str) -> String {
    // Data only. pi does not append advice here either: the block exists so the model can
    // see the facts, and telling it what to do with them would be a prompt by another name.
    format!(
        "<environment>\n工作目录: {}\n平台: {}\nshell: {}\n会话开始: {}\n会话 id: {}\n</environment>",
        cwd.display(),
        std::env::consts::OS,
        shell,
        crate::agent::session::now(),
        session_id
    )
}

/// The per-session environment block, which is stored as the first user message.
///
/// Several places need to tell it apart from something the user actually typed: resuming
/// must not echo it back as if it were user input, and the `/resume` list must not quote it
/// as the session's headline.
pub fn is_environment_block(message: &Message) -> bool {
    message.text().trim_start().starts_with("<environment>")
}

/// What the user typed.
pub enum Input {
    Line(String),
    Exit,
}

/// The outcome of one assistant turn.
enum TurnEnd {
    /// The model finished and there is nothing left to do.
    Done,
    /// Tool results were produced; keep going.
    Continue,
}

pub struct Agent {
    pub config: Config,
    pub client: Client,
    pub screen: Screen,
    pub session: Session,
    gate: PermissionGate,
    pub cwd: PathBuf,
    model_spec: String,
    level: String,
    compaction: CompactionState,
    retry: RetryBudget,
    /// Set while a turn is streaming, so `/compact` can refuse instead of corrupting it.
    streaming: bool,
    /// The session's system message, read from `AGENTS.md` when the session started.
    ///
    /// `None` means "send no system message". Held rather than re-read so the prefix stays
    /// byte-identical for the life of the session even if the file changes underneath.
    system_prompt: Option<String>,
    /// Set by `/delete`: there is no file left to report as saved on the way out.
    deleted: bool,
}

impl Agent {
    pub fn new(config: Config, cwd: PathBuf, interactive: bool) -> anyhow::Result<Self> {
        let model_spec = config
            .default_model
            .clone()
            .ok_or_else(|| anyhow::anyhow!("配置里没有可用的模型"))?;
        let level = {
            let (_, model) = config
                .find(&model_spec)
                .ok_or_else(|| anyhow::anyhow!("default_model「{model_spec}」不存在"))?;
            model.levels().first().cloned().unwrap_or_default()
        };
        let dialect = policy::configured_dialect(&config.shell.path);
        let session = Session::create(&cwd, &model_spec)?;
        let client = Client::new()?;
        let mut screen = Screen::new();
        screen.set_commands(COMMANDS);
        // The session id is deliberately *not* announced here: it is printed on the way out,
        // as part of the command that resumes it, which is the only time it is useful.
        // No `AGENTS.md` means no system message at all: pi has no prompt of its own.
        let system_prompt = system_prompt_from(&cwd);
        if let Some((path, text)) = load_agents_md(&cwd) {
            screen.push_lines(ui_compact::note_lines(
                &format!("系统提示词：{}（{} 字）", util::shorten_home(&path, dirs::home_dir().as_deref()), text.chars().count()),
                crate::ui::screen::Style::new(Color::Dim),
            ));
        }
        // The environment block becomes the head of the conversation and is persisted with
        // the session. It sits after the system message, which is prepended per request.
        let block = environment_block(&cwd, &session.header().id, &config.shell.path);
        let mut session = session;
        session.push_message(Message::user_text(block), None, None)?;
        Ok(Agent {
            config,
            client,
            screen,
            session,
            gate: PermissionGate::new(interactive, dialect),
            cwd,
            model_spec,
            level,
            compaction: CompactionState::default(),
            retry: RetryBudget::default(),
            streaming: false,
            system_prompt,
            deleted: false,
        })
    }

    /// Resume an existing session file.
    pub fn resume(config: Config, cwd: PathBuf, path: &Path, interactive: bool) -> anyhow::Result<Self> {
        let mut session = Session::open(path)?;
        let model_spec = session
            .header()
            .model
            .clone();
        let model_spec = if config.find(&model_spec).is_some() {
            model_spec
        } else {
            config
                .default_model
                .clone()
                .ok_or_else(|| anyhow::anyhow!("配置里没有可用的模型"))?
        };
        let level = {
            let (_, model) = config.find(&model_spec).unwrap();
            model.levels().first().cloned().unwrap_or_default()
        };
        let dialect = policy::configured_dialect(&config.shell.path);
        let client = Client::new()?;
        let mut screen = Screen::new();
        screen.set_commands(COMMANDS);
        // Read from the directory the session is being resumed in: the instructions are
        // about the code being worked on, and that is where the work happens now.
        let system_prompt = system_prompt_from(&cwd);
        let name = session.name().unwrap_or_else(|| "未命名".into());
        screen.push_lines(ui_compact::note_lines(
            &format!("已恢复会话 {name}（{} 条消息）", session.context_messages().len()),
            crate::ui::screen::Style::new(Color::Dim),
        ));
        // Replay the transcript so the user can see where the work stopped. The whole
        // conversation is replayed, not just the user's lines: a resume that showed only the
        // questions would look like the answers were lost. Environment blocks are skipped —
        // they are bookkeeping, and the newest one is written back below if the directory
        // changed.
        for block in ui_compact::replay_blocks(&session.context_messages()) {
            screen.push(block);
        }
        // The working directory travels with the session: the newest environment block names
        // it, so running the tools somewhere else would make the transcript lie about where
        // it is. The header records where the session *started*, which is not the same thing
        // once it has been resumed elsewhere.
        let recorded = session.current_cwd().unwrap_or_else(|| PathBuf::from(&session.header().cwd));
        if recorded == cwd {
            // Same directory as before: nothing to say and nothing to write.
        } else {
            let missing = !recorded.is_dir();
            screen.push_lines(ui_compact::note_lines(
                &if missing {
                    format!(
                        "会话原目录 {} 已不存在，本次在 {} 继续；已写入新的环境信息。",
                        recorded.display(),
                        cwd.display()
                    )
                } else {
                    format!(
                        "会话原目录 {}，本次在 {} 继续；已写入新的环境信息。",
                        recorded.display(),
                        cwd.display()
                    )
                },
                crate::ui::screen::Style::new(Color::Dim),
            ));
            // Append a fresh block instead of rewriting the old one: the file is append-only,
            // and the newest block is the one the model should trust.
            let block = environment_block(&cwd, &session.header().id, &config.shell.path);
            session.push_message(Message::user_text(block), None, None)?;
            session.relocate(&cwd)?;
        }
        screen.flush();
        Ok(Agent {
            config,
            client,
            screen,
            session,
            gate: PermissionGate::new(interactive, dialect),
            cwd,
            model_spec,
            level,
            compaction: CompactionState::default(),
            retry: RetryBudget::default(),
            streaming: false,
            system_prompt,
            deleted: false,
        })
    }

    pub fn model_spec(&self) -> &str {
        &self.model_spec
    }

    pub fn level(&self) -> &str {
        &self.level
    }

    fn model(&self) -> anyhow::Result<(&Provider, &ModelConfig)> {
        self.config
            .find(&self.model_spec)
            .ok_or_else(|| anyhow::anyhow!("模型「{}」不在配置里", self.model_spec))
    }

    /// Read one line, updating the footer around the call.
    pub fn read_input(&mut self) -> Action {
        // Everything queued above is committed here, so the prompt always appears below a
        // complete transcript.
        self.render_footer(None);
        let action = self.screen.read_input();
        match action {
            Ok(action) => action,
            Err(err) => {
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("读取输入失败：{err}"),
                    crate::ui::screen::Style::new(Color::Red),
                ));
                Action::Eof
            }
        }
    }

    /// Draw the two-line footer (plus a status row when something is in flight).
    pub fn render_footer(&mut self, busy: Option<&str>) {
        let (model, level) = match self.model() {
            Ok((_, model)) => (Some(model), self.level.clone()),
            Err(_) => (None, self.level.clone()),
        };
        let cache_hit = self.session.totals.hit_rate();
        let context_tokens = self.session.last_usage.as_ref().map(|usage| {
            usage.input + usage.output + usage.cache_read + usage.cache_write
        });
        let context_window = model.and_then(|model| model.context_window);
        let state = FooterState {
            cwd: &self.cwd,
            branch: footer::git_branch(&self.cwd),
            session_name: self.session.name(),
            totals: self.session.totals,
            cache_hit_rate: cache_hit,
            context_tokens: if self.compaction.tokens_unknown { None } else { context_tokens },
            context_window,
            model,
            level: &level,
            compacting: self.compaction.running,
            busy,
        };
        let lines = footer::render(&state, &self.screen.theme, self.screen.width());
        self.screen.set_footer(lines);
        // The session name and directory both live in the footer, so the window title is
        // refreshed wherever the footer is — including a session switch.
        self.screen.set_title(self.session.name().as_deref(), &self.cwd);
        self.screen.render();
    }

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

    fn command_name(&mut self, argument: &str) -> anyhow::Result<()> {
        // `/name` without an argument clears the name, exactly like pi.
        let name = if argument.is_empty() {
            None
        } else {
            Some(argument.replace('\n', " "))
        };
        self.session.set_name(name.as_deref())?;
        let note = match &name {
            Some(name) => format!("会话名已设为「{}」", util::truncate(name, Defaults::SESSION_NAME_WIDTH, "…")),
            None => "会话名已清空".to_string(),
        };
        self.screen.push_lines(ui_compact::note_lines(&note, crate::ui::screen::Style::new(Color::Dim)));
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
    fn command_delete(&mut self) -> anyhow::Result<bool> {
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
        let choice = self.screen.pick(
            &format!("删除会话「{name}」？"),
            &["不删，继续".to_string(), "是的，删除并退出".to_string()],
        );
        // The safe option is highlighted first, so a reflexive Enter keeps the session.
        if choice != Some(1) {
            self.screen.push_lines(ui_compact::note_lines(
                "已取消，会话保留。",
                crate::ui::screen::Style::new(Color::Dim),
            ));
            return Ok(true);
        }
        let removed = self.session.delete()?;
        self.deleted = true;
        // The store's entry goes with the last session in it. Otherwise a directory stays
        // listed for every project that was ever used, and the store stops being a list of
        // where history *is* — which is the whole reason for grouping by directory.
        let forgotten = crate::config::forget_dir_if_empty(&self.cwd);
        let note = if removed {
            if forgotten {
                format!("已删除会话文件：{path}")
            } else {
                // Other sessions remain here, so the directory stays.
                format!("已删除会话文件：{path}（本目录还有其它会话）")
            }
        } else {
            // A session that never said anything has no file: there was nothing to delete,
            // and reporting a path as deleted would be false.
            "本会话没有内容，未生成文件。".to_string()
        };
        self.screen.push_lines(ui_compact::note_lines(
            &note,
            crate::ui::screen::Style::new(Color::Dim),
        ));
        Ok(false)
    }

    /// Whether `/delete` removed the session file.
    pub fn session_deleted(&self) -> bool {
        self.deleted
    }

    /// The session, for the few questions the loop asks about it on the way out.
    pub fn session(&self) -> &Session {
        &self.session
    }

    fn command_new(&mut self) -> anyhow::Result<()> {
        // The id is not announced: it is printed on the way out, with the command that
        // resumes it, which is the only place it is useful.
        self.start_new_session("新会话开始。")
    }

    /// Replace the current session with a fresh one, keeping the working directory.
    ///
    /// The old session is not deleted: it is on disk and still reachable through `/resume`,
    /// so "new" costs nothing and undo is a resume away.
    fn start_new_session(&mut self, note: &str) -> anyhow::Result<()> {
        let model_spec = self.model_spec.clone();
        self.session = Session::create(&self.cwd, &model_spec)?;
        self.system_prompt = system_prompt_from(&self.cwd);
        let block = environment_block(&self.cwd, &self.session.header().id, &self.config.shell.path);
        self.session.push_message(Message::user_text(block), None, None)?;
        self.gate.reset();
        self.screen.clear_transcript();
        self.screen
            .push_lines(ui_compact::note_lines(note, crate::ui::screen::Style::new(Color::Dim)));
        Ok(())
    }

    async fn command_resume(&mut self) -> anyhow::Result<()> {
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
        // is lost; the confirmation exists because "I pressed Esc" does not obviously mean
        // "discard the screen I am looking at".
        let Some(index) = self.screen.pick_with_hint(
            "恢复历史会话",
            "↑↓ 选择 · Enter 确认 · Esc 开始新会话",
            &items,
        ) else {
            self.start_new_session("已按 Esc：新会话开始。")?;
            return Ok(());
        };
        let target = summaries[index].path.clone();
        if target == self.session.path() {
            return Ok(());
        }
        // Flush anything pending before pointing the loop at another file.
        let model_spec = self.model_spec.clone();
        match Session::open(&target) {
            Ok(session) => {
                self.session = session;
                self.gate.reset();
                self.screen.clear_transcript();
                // The switched-to session may have started under a different `AGENTS.md`,
                // so the prompt is re-read here too.
                self.system_prompt = system_prompt_from(&self.cwd);
                let name = self.session.name().unwrap_or_else(|| "未命名".into());
                self.screen.push_lines(ui_compact::note_lines(
                    &format!("已切到会话「{name}」"),
                    crate::ui::screen::Style::new(Color::Dim),
                ));
                if self.config.find(&model_spec).is_some() {
                    self.model_spec = model_spec;
                }
                // Same replay as a start-up resume: the switched-to session has to look
                // like the session it is, answers included.
                for block in ui_compact::replay_blocks(&self.session.context_messages()) {
                    self.screen.push(block);
                }
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
    async fn command_model(&mut self) -> anyhow::Result<()> {
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
        let model_name = model.display_name().to_string();
        let levels = model.levels();
        let mut note = format!("已切换到 {model_name}");

        if levels.is_empty() {
            self.level.clear();
            note.push_str("（该模型不支持推理）");
        } else {
            let current = levels.iter().position(|level| *level == self.level).unwrap_or(0);
            let level_items: Vec<String> = levels.iter().map(|level| level.to_string()).collect();
            if let Some(chosen) = self.screen.pick_at("思考级别", &level_items, current) {
                self.level = levels[chosen].clone();
            }
            // A level that came from another model may not exist here.
            let (clamped, moved) = llm::clamp_level(model, &self.level);
            if moved {
                note.push_str(&format!(" · 思考级别 {level} 不受支持，已调整为 {clamped}", level = self.level));
            }
            self.level = clamped;
            note.push_str(&format!(" · {}", self.level));
        }
        self.screen.push_lines(ui_compact::note_lines(&note, crate::ui::screen::Style::new(Color::Dim)));
        Ok(())
    }

    /// `/compact`: always allowed on request, but it reports the reason when there is
    /// nothing to cut.
    async fn command_compact(&mut self, instructions: &str) -> anyhow::Result<()> {
        if self.streaming {
            self.screen.push_lines(ui_compact::note_lines(
                "正在流式输出，无法压缩；等这一轮结束后再试。",
                crate::ui::screen::Style::new(Color::Yellow),
            ));
            return Ok(());
        }
        let custom = (!instructions.trim().is_empty()).then_some(instructions);
        // Summarising the history is another long request, and outside a turn there is no
        // spinner running already.
        self.screen.set_working(WORKING_LABEL);
        match self.compact(Reason::Manual, custom).await {
            Ok(()) => {}
            Err(err) => {
                self.screen.push_lines(ui_compact::note_lines(
                    &err.to_string(),
                    crate::ui::screen::Style::new(Color::Yellow),
                ));
            }
        };
        self.screen.clear_working();
        Ok(())
    }

    /// Run one compaction and write the checkpoint.
    async fn compact(&mut self, reason: Reason, custom: Option<&str>) -> Result<(), CompactError> {
        if self.compaction.running {
            return Err(CompactError::InProgress);
        }
        self.compaction.begin(reason)?;
        self.render_footer(Some(reason.banner()));
        let result = self.compact_inner(reason, custom).await;
        self.compaction.finish();
        self.render_footer(None);
        result
    }

    async fn compact_inner(&mut self, reason: Reason, custom: Option<&str>) -> Result<(), CompactError> {
        let (provider, model) = self
            .model()
            .map_err(|err| CompactError::Summarize(err.to_string()))?;
        let messages = self.session.context_messages();
        let previous = self.session.last_checkpoint_index().and_then(|index| {
            match &self.session.records()[index] {
                crate::agent::session::Record::Compacted { summary, .. } => Some(summary.clone()),
                _ => None,
            }
        });
        let real_usage = self.session.last_usage.as_ref().map(|usage| {
            usage.input + usage.output + usage.cache_read + usage.cache_write
        });
        let request = SummaryRequest {
            provider,
            model,
            session_id: &self.session.header().id,
            previous_summary: previous.as_deref(),
            custom_instructions: custom,
            level: &self.level,
        };
        // The keep-recent window is scaled to the model's capacity: keeping a fixed 20k in a
        // 6k window would produce a checkpoint larger than the history it replaced.
        let keep_recent = model
            .context_window
            .map(compact::keep_recent_for)
            .unwrap_or(Defaults::KEEP_RECENT_TOKENS);
        let outcome = compact::run(
            &self.client,
            request,
            &messages,
            self.system_prompt.as_deref().unwrap_or_default(),
            keep_recent,
            real_usage,
        )
        .await?;
        // A compaction that did not actually shrink anything is worth saying out loud: it
        // means the summary request cost more than it saved.
        let grew = outcome.tokens_after >= outcome.tokens_before;
        self.session
            .push_compaction(
                reason.label(),
                &outcome.summary,
                outcome.replacement.clone(),
                outcome.read_files.clone(),
                outcome.modified_files.clone(),
                Some(outcome.usage),
            )
            .map_err(|err| CompactError::Session(err.to_string()))?;
        let saved = outcome.tokens_before.saturating_sub(outcome.tokens_after);
        let note = if grew {
            // Compaction is not free; if it did not help, the user should know that the model's
            // window is too small for the summary to pay for itself.
            format!(
                "已压缩上下文（{}）：约 {} → {} token，这次没有变小；\n\
                 该模型窗口偏小，摘要本身的开销超过了省下的量。",
                reason.label(),
                util::fmt_tokens(outcome.tokens_before, true),
                util::fmt_tokens(outcome.tokens_after, true),
            )
        } else {
            format!(
                "已压缩上下文（{}）：约 {} → {} token（省下约 {}）",
                reason.label(),
                util::fmt_tokens(outcome.tokens_before, true),
                util::fmt_tokens(outcome.tokens_after, true),
                util::fmt_tokens(saved, true),
            )
        };
        self.screen
            .push_lines(ui_compact::note_lines(&note, crate::ui::screen::Style::new(Color::Dim)));
        Ok(())
    }

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
    async fn assistant_turn(&mut self) -> anyhow::Result<TurnEnd> {
        let (provider, model, compat) = {
            let (provider, model) = self.model()?;
            let compat = provider.compat(model);
            (provider.clone(), model.clone(), compat)
        };
        // Threshold compaction runs before the request, using real usage when it is still
        // valid and an estimate otherwise.
        if let Some(window) = model.context_window {
            let limit = compact::threshold_for(window);
            let messages = self.session.context_messages();
            let real_usage = self.session.last_usage.as_ref().map(|usage| {
                usage.input + usage.output + usage.cache_read + usage.cache_write
            });
            let used = compact::estimate_context(
                &messages,
                self.system_prompt.as_deref().unwrap_or_default(),
                real_usage,
            );
            if used > limit {
                self.screen.push_lines(ui_compact::note_lines(
                    &format!(
                        "上下文已用 {}/{}，接近上限，正在压缩…",
                        util::fmt_tokens(used, true),
                        util::fmt_tokens(window, true)
                    ),
                    crate::ui::screen::Style::new(Color::Yellow),
                ));
                if let Err(err) = self.compact(Reason::Threshold, None).await {
                    // A failed automatic compaction must not lose the turn; the request
                    // may still fit, and if it does not the overflow path will try again.
                    self.screen.push_lines(ui_compact::note_lines(
                        &format!("自动压缩未完成：{err}"),
                        crate::ui::screen::Style::new(Color::Yellow),
                    ));
                }
            }
        }

        let mut messages = self.session.context_messages();
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
        let completion = {
            let mut sink = |delta: Delta| {
                let _ = sender.send(delta);
            };
            let stream = self.client.stream(&request, &mut sink);
            tokio::pin!(stream);
            let mut ticker = ticker();
            let result = loop {
                // Keep the input line alive while the answer arrives. The user types into
                // the composer as they read; Enter there queues the message rather than
                // dropping it, because a turn cannot be interrupted mid-request without
                // throwing away what the model is halfway through saying.
                if let Some(action) = self.screen.poll_input() {
                    on_turn_action(&mut self.screen, action);
                }
                tokio::select! {
                    // A token outranks the spinner: text has to appear as it arrives, not on
                    // the next frame. Everything already queued is drained with it, so a burst
                    // of tokens costs one redraw rather than one per token.
                    biased;
                    Some(delta) = deltas.recv() => {
                        apply_delta(&mut self.screen, delta);
                        drain_deltas(&mut self.screen, &mut deltas);
                    }
                    _ = ticker.tick() => self.screen.tick_working(),
                    out = &mut stream => break out,
                }
            };
            drain_deltas(&mut self.screen, &mut deltas);
            result
        };
        self.streaming = false;
        // On a transport failure the streamed preview is discarded, so the transcript does
        // not show a half-written answer that was never recorded.
        let completion = match completion {
            Ok(completion) => completion,
            Err(err) => {
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
        // are compacted here, before the message is recorded.
        let overflow = compact::detect_overflow(&completion, model.context_window, model.max_tokens());
        if let Some(signal) = overflow {
            self.screen.push_lines(ui_compact::note_lines(
                &describe_overflow(&signal),
                crate::ui::screen::Style::new(Color::Yellow),
            ));
            let retried = self.recover_overflow("").await?;
            if let Some(end) = retried {
                return Ok(end);
            }
            // A response that finished successfully cannot be continued by resending, so
            // the compaction alone is the recovery; nothing else to retry.
            return Ok(TurnEnd::Done);
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

    /// Decide what to do after a response that is not an overflow.
    fn after_completion(&mut self, completion: &llm::Completion) -> TurnEnd {
        let calls = completion.tool_calls();
        if calls.is_empty() {
            // A length stop with no tool calls means the answer was cut off; say so rather
            // than silently pretending it finished.
            if completion.stop_reason == StopReason::Length {
                self.screen.push_lines(ui_compact::note_lines(
                    "输出达到长度上限，回答可能不完整。可以继续要求补全。",
                    crate::ui::screen::Style::new(Color::Yellow),
                ));
            }
            return TurnEnd::Done;
        }
        // Tool calls are executed inline; the next request carries their results.
        self.execute_tools(&calls);
        TurnEnd::Continue
    }

    /// Execute every tool call from one assistant message, in order.
    ///
    /// A refused call still produces a result: the model receives the refusal text with
    /// the policy's reason, and the transcript shows the same failure. Skipping the tool
    /// result entirely would break the call/result pairing the providers expect.
    fn execute_tools(&mut self, calls: &[(String, String, serde_json::Value)]) {
        let cwd = self.cwd.clone();
        for (id, name, arguments) in calls {
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
                    let result = block_on_spinning(&mut self.screen, tools::execute(name, arguments, &cwd));
                    self.gate.finish(id);
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
        }
    }

    /// The single overflow recovery attempt for this turn. Returns `Some(..)` when the turn
    /// was handled here, `None` when the caller should carry on with normal error handling.
    async fn recover_overflow(&mut self, error_text: &str) -> anyhow::Result<Option<TurnEnd>> {
        let (_, model) = self.model()?;
        let is_overflow = error_text.is_empty()
            || compact::looks_like_overflow(error_text);
        if !is_overflow || !self.retry.available() || model.context_window.is_none() {
            return Ok(None);
        }
        self.retry.spend();
        self.screen.push_lines(ui_compact::note_lines(
            "上下文超限，正在压缩后重试…",
            crate::ui::screen::Style::new(Color::Yellow),
        ));
        // Drop the failed assistant message before compacting: keeping it would fold a
        // half-written answer into the summary.
        self.session.drop_last_assistant()?;
        if let Err(err) = self.compact(Reason::Overflow, None).await {
            self.screen.push_lines(ui_compact::note_lines(
                &format!("压缩失败：{err}。请减少上下文或换用窗口更大的模型。"),
                crate::ui::screen::Style::new(Color::Red),
            ));
            return Ok(Some(TurnEnd::Done));
        }
        Ok(Some(TurnEnd::Continue))
    }
}

/// Handle what the user did while a turn was running.
///
/// Only a few things make sense mid-turn. A submitted line is *queued*, not run: the model is
/// answering the previous message, and starting a second turn underneath it would interleave
/// two conversations. Typing is taken by the composer and never reaches here.
fn on_turn_action(screen: &mut Screen, action: crate::ui::screen::Action) {
    use crate::ui::screen::Action;
    match action {
        Action::Line(text) => queue_mid_turn(screen, text, Vec::new()),
        Action::LineWithImages(text, images) => queue_mid_turn(screen, text, images),
        // Ctrl+O is exactly what a user does while a long tool call is on screen.
        Action::ToggleExpand => {
            if !screen.toggle_last_collapsible() {
                screen.push_lines(ui_compact::note_lines(
                    "没有可展开的内容。",
                    crate::ui::screen::Style::new(Color::Dim),
                ));
            }
        }
        // Ctrl+C / Ctrl+D during a turn do nothing: the only thing they could stop is the
        // request, and throwing away a half-written answer loses more than it saves.
        Action::Interrupt | Action::Eof => {}
    }
}

/// Hold a submitted line until the turn in flight is over.
///
/// A line starting with `/` is a command, and commands are dispatched as commands, never as
/// messages: `/model` is not something the user said to the model. It is the same rule the
/// prompt applies — a slash command is text-only, so any image submitted with it is dropped
/// rather than smuggled into the conversation as prose.
///
/// Everything else is a message and waits as one, keeping its images: the pictures were
/// pasted for that message, and the model has to see them with it.
pub(crate) fn queue_mid_turn(
    screen: &mut Screen,
    text: String,
    images: Vec<crate::image_input::PastedImage>,
) {
    use crate::ui::screen::Queued;
    // Trimmed, because the prompt trims: a line goes into the conversation the same way
    // whether it was typed at the prompt or during a turn, and a leading space is not part
    // of what the user meant to say.
    let text = text.trim();
    if text.is_empty() && images.is_empty() {
        return;
    }
    // A leading `/` makes it a command, exactly as at the prompt: ` /model` with a space in
    // front is not a command there either.
    if text.starts_with('/') {
        screen.queue(Queued::Command(text.to_string()));
    } else {
        screen.queue(Queued::Message(text.to_string(), images));
    }
}

/// Drive a future to completion on a private current-thread runtime.
///
/// Tool execution is synchronous by nature (spawn a process, read a file) while the agent
/// loop is async. Rather than colour everything async, the two points where sync code has
/// to wait on async work go through here — and both of them complete without ever yielding
/// to the outer runtime, so blocking the thread is safe and predictable.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a private runtime")
            .block_on(future)
    })
}

/// Drive a future to completion, advancing the spinner while it runs.
///
/// The future must not borrow the screen: the spinner needs it on every tick. That is the
/// whole reason the delta sink goes through a channel instead of writing directly.
fn block_on_spinning<T>(screen: &mut Screen, work: impl std::future::Future<Output = T>) -> T {
    block_on(async {
        tokio::pin!(work);
        let mut ticker = ticker();
        loop {
            // A command can run for minutes, which is when the input line has to stay
            // usable: a directory that takes a minute to list is a minute the user would
            // otherwise spend watching it.
            if let Some(action) = screen.poll_input() {
                on_turn_action(screen, action);
            }
            tokio::select! {
                out = &mut work => break out,
                _ = ticker.tick() => screen.tick_working(),
            }
        }
    })
}

/// A ticker that fires every [`WORKING_INTERVAL`], starting one interval from now.
///
/// `interval` would fire its first tick immediately, which draws the second frame before
/// the first has been seen. Missed ticks are delayed rather than burst: after a long block
/// a burst of catch-up frames would animate nothing but the backlog.
fn ticker() -> tokio::time::Interval {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + WORKING_INTERVAL,
        WORKING_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker
}

/// Show one delta, exactly as a direct sink would have.
fn apply_delta(screen: &mut Screen, delta: Delta) {
    match delta {
        Delta::Text(text) => screen.push_text(&text),
        Delta::Thinking(text) => screen.push_thinking(&text),
    }
}

/// Move every delta the stream has produced so far into the screen.
fn drain_deltas(screen: &mut Screen, deltas: &mut tokio::sync::mpsc::UnboundedReceiver<Delta>) {
    while let Ok(delta) = deltas.try_recv() {
        apply_delta(screen, delta);
    }
}

/// Exposed for tests: the dialect pi will hand to the policy.
pub fn dialect_for(config: &Config) -> Dialect {
    policy::configured_dialect(&config.shell.path)
}

/// Human-readable form of an overflow signal, shown before the compaction starts.
pub fn describe_overflow(signal: &compact::OverflowSignal) -> String {
    match signal {
        compact::OverflowSignal::ExplicitError => "上游报告上下文超限，正在压缩后重试…".to_string(),
        compact::OverflowSignal::SilentOverflow { prompt_tokens, context_window } => format!(
            "输入 {}/{} token 已超出窗口，正在压缩后重试…",
            util::fmt_tokens(*prompt_tokens, true),
            util::fmt_tokens(*context_window, true)
        ),
        compact::OverflowSignal::LengthCut { output_tokens, max_tokens } => format!(
            "输出只剩 {}/{max_tokens} token，疑似上下文挤占，正在压缩后重试…",
            output_tokens
        ),
    }
}

/// `AGENTS.md` filenames, in the order pi looks for them.
const AGENTS_FILES: &[&str] = &["AGENTS.md", "AGENTS.MD"];
/// The project root: the nearest directory at or above `cwd` that holds a `.git`, or `cwd`
/// itself when there is none.
///
/// `.git` may be a directory (an ordinary checkout) or a file (a submodule or a linked
/// worktree), so only its presence is checked.
fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// Read the project's `AGENTS.md`, and only that one.
///
/// The file is looked up in the project root — not in `cwd`, and not in any directory above
/// the root. A checkout can be run from anywhere inside it and the instructions are the ones
/// the project committed; conversely, a stray file in `/tmp` or in the parent of the project
/// cannot inject instructions, which is what makes the lookup safe to do without asking.
///
/// Nothing is read when the file does not exist: pi has no built-in prompt, so the model
/// gets a system message only if the project asked for one.
pub fn load_agents_md(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = project_root(cwd);
    for name in AGENTS_FILES {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        return match std::fs::read_to_string(&path) {
            // Whitespace-only counts as absent: it would otherwise send an empty system
            // message on every request.
            Ok(text) if text.trim().is_empty() => None,
            Ok(text) => {
                let label = path.display().to_string();
                Some((path, format!("<!-- {label} -->\n{}", text.trim_end())))
            }
            Err(err) => {
                eprintln!("pi: 读取 {} 失败：{err}", path.display());
                None
            }
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_no_built_in_prompt() {
        // pi ships no prompt of its own: a directory without `AGENTS.md` sends no system
        // message at all, rather than a default one nobody asked for.
        let dir = std::env::temp_dir().join(format!("pi-no-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_agents_md(&dir).is_none());
        assert!(system_prompt_from(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_project_root_supplies_the_prompt() {
        // Running from a subdirectory still picks up the project's own file, and a nested
        // file below the root is not consulted: the root is what was committed.
        let root = std::env::temp_dir().join(format!("pi-agents-{}", std::process::id()));
        let nested = root.join("crates/inner");
        let _ = std::fs::remove_dir_all(&root);
        // Only the project root is a checkout; the nested directory is a plain subdirectory,
        // so the root is where the lookup stops.
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "根目录规则").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "内层规则").unwrap();

        let (path, text) = load_agents_md(&nested).expect("the root file is found");
        assert_eq!(path, root.join("AGENTS.md"));
        assert!(text.contains("根目录规则"));
        assert!(!text.contains("内层规则"), "a nested file must not be read: {text:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_above_the_project_root_is_read() {
        // The lookup stops at the root. Without that boundary, a directory the user does not
        // control — a shared `/tmp`, another user's home — could inject instructions into the
        // prompt of a project that never asked for them.
        let outer = std::env::temp_dir().join(format!("pi-outer-{}", std::process::id()));
        let project = outer.join("project");
        let _ = std::fs::remove_dir_all(&outer);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(outer.join("AGENTS.md"), "外部注入").unwrap();
        // No `.git` under the project: the root is the working directory itself.
        assert!(load_agents_md(&project).is_none(), "the outer file must be ignored");

        // With a `.git`, the root is the project, and the outer file is still ignored.
        std::fs::create_dir(project.join(".git")).unwrap();
        std::fs::write(project.join("AGENTS.md"), "本项目规则").unwrap();
        let (_, text) = load_agents_md(&project).expect("the project file is found");
        assert!(!text.contains("外部注入"), "{text:?}");

        let _ = std::fs::remove_dir_all(&outer);
    }

    #[test]
    fn a_project_without_a_git_directory_uses_the_working_directory() {
        // A plain directory is its own project, so its file is read — the rule is "the root
        // of what you are working on", not "only git checkouts".
        let dir = std::env::temp_dir().join(format!("pi-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "规则").unwrap();
        assert!(load_agents_md(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_agents_md_is_treated_as_absent() {
        // A placeholder file must not produce an empty system message: it would be sent on
        // every request and would say nothing.
        let dir = std::env::temp_dir().join(format!("pi-empty-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "   \n\t\n").unwrap();
        assert!(system_prompt_from(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_environment_block_is_separate_from_the_system_prompt() {
        let cwd = Path::new("/tmp/project");
        let block = environment_block(cwd, "abc123", "/usr/bin/zsh");
        assert!(block.contains("/tmp/project"));
        assert!(block.contains("/usr/bin/zsh"));
        assert!(block.contains("abc123"));
        assert!(block.starts_with("<environment>"));
    }

    #[test]
    fn the_environment_block_is_stable_for_the_same_inputs() {
        let cwd = Path::new("/tmp/project");
        let first = environment_block(cwd, "id", "/usr/bin/zsh");
        let second = environment_block(cwd, "id", "/usr/bin/zsh");
        // Only the "session start" line may differ, and only if the clock ticks between
        // the two calls; everything else must be identical.
        let strip = |text: &str| {
            text.lines()
                .filter(|line| !line.starts_with("会话开始:"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&first), strip(&second));
    }

    #[test]
    fn the_dialect_comes_from_the_configured_shell() {
        let mut config = Config::default();
        config.shell.path = "/usr/bin/zsh".into();
        assert_eq!(dialect_for(&config), Dialect::Zsh);
        config.shell.path = "/bin/bash".into();
        assert_eq!(dialect_for(&config), Dialect::Bash);
    }

}
