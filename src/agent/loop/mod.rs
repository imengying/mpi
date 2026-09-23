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
    /// The user stopped the turn with Esc while its tools were running.
    Stopped,
}

/// What one streamed request produced.
///
/// Esc is not a dropped future but a recorded decision: the loop breaks on a stop with no
/// completion to return, and the caller has to tell that apart from a request that failed.
struct TurnOutcome {
    result: Option<Result<llm::Completion, llm::LlmError>>,
}

impl TurnOutcome {
    /// Whether the request was ended by Esc rather than by the server or by an error.
    fn stopped(&self) -> bool {
        self.result.is_none()
    }
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
        // Read once for both uses, so the note below and the message sent cannot disagree.
        let agents = load_agents_md(&cwd);
        let system_prompt = agents.as_ref().map(|(_, text)| text.clone());
        // Which file shapes the session is invisible otherwise and worth a line; how long it
        // is, is not — the file is one `read` away if that matters.
        if let Some((path, _)) = &agents {
            screen.push_lines(ui_compact::note_lines(
                &format!("系统提示词：{}", util::shorten_home(path, dirs::home_dir().as_deref())),
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
        // The model and level come from the newest turn the session recorded, falling back to
        // the header for a session old enough to predate turn contexts. Taking the header
        // always would silently undo `/model`: the choice was made in the conversation and
        // the conversation has to remember it.
        let (recorded_model, recorded_level) = session.current_model().unwrap_or_default();
        let model_spec = match recorded_model {
            spec if !spec.is_empty() => spec,
            _ => session.header().model.clone(),
        };
        let model_spec = if config.find(&model_spec).is_some() {
            model_spec
        } else {
            config
                .default_model
                .clone()
                .ok_or_else(|| anyhow::anyhow!("配置里没有可用的模型"))?
        };
        // The level is only meaningful for the model it was recorded with, and a config edit
        // can remove it; clamping against the model being resumed keeps a level that no
        // longer exists from being sent.
        let level = {
            let (_, model) = config.find(&model_spec).unwrap();
            if recorded_level.is_empty() {
                model.levels().first().cloned().unwrap_or_default()
            } else {
                crate::llm::clamp_level(model, &recorded_level).0
            }
        };
        let dialect = policy::configured_dialect(&config.shell.path);
        let client = Client::new()?;
        let mut screen = Screen::new();
        screen.set_commands(COMMANDS);
        // Read from the directory the session is being resumed in: the instructions are
        // about the code being worked on, and that is where the work happens now.
        let system_prompt = system_prompt_from(&cwd);
        // The transcript below is the session, so the note does not repeat what it says:
        // only the name it goes under, which the replay itself does not carry.
        let name = session.name().unwrap_or_else(|| "未命名".into());
        screen.push_lines(ui_compact::note_lines(
            &format!("已恢复会话 {name}"),
            crate::ui::screen::Style::new(Color::Dim),
        ));
        // Replay the transcript so the user can see where the work stopped. The whole
        // conversation is replayed, not just the user's lines: a resume that showed only the
        // questions would look like the answers were lost. Environment blocks are skipped —
        // they are bookkeeping, and the newest one is written back below if the directory
        // changed.
        for block in ui_compact::replay_blocks(&session.context_messages(), screen.width()) {
            screen.push(block);
        }
        // The arrows reach back into the conversation, not just into this process: Up on a
        // resumed session has to find the message that was typed before the resume, or the
        // turns already in the file look like they were never typed.
        screen.seed_history(session.user_history());
        // The working directory travels with the session: the newest environment block names
        // it, so running the tools somewhere else would make the transcript lie about where
        // it is. The header records where the session *started*, which is not the same thing
        // once it has been resumed elsewhere.
        let recorded = session.current_cwd().unwrap_or_else(|| PathBuf::from(&session.header().cwd));
        if recorded == cwd {
            // Same directory as before: nothing to say and nothing to write.
        } else {
            // The old directory is named because it is the reason the transcript's paths no
            // longer resolve here; whether it still exists tells the user which of the two
            // ways it went. How the environment block was refreshed is bookkeeping.
            let missing = !recorded.is_dir();
            screen.push_lines(ui_compact::note_lines(
                &if missing {
                    format!(
                        "会话原目录 {} 已不存在，本次在 {} 继续。",
                        recorded.display(),
                        cwd.display()
                    )
                } else {
                    format!(
                        "会话原目录 {}，本次在 {} 继续。",
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
        let mut agent = Agent {
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
        };
        // Draw the footer now rather than waiting for the first prompt. It is the row that
        // says which model and level the session came back with, and until the first
        // `read_input` it would otherwise sit empty — the one thing a user checking "did my
        // model stick?" looks at would be the one thing not on screen.
        agent.render_footer(None);
        Ok(agent)
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

    /// Whether `/delete` removed the session file.
    pub fn session_deleted(&self) -> bool {
        self.deleted
    }

    /// The session, for the few questions the loop asks about it on the way out.
    pub fn session(&self) -> &Session {
        &self.session
    }
}

/// Handle what the user did while a turn was running.
///
/// Only a few things make sense mid-turn. A submitted line is *queued*, not run: the model is
/// answering the previous message, and starting a second turn underneath it would interleave
/// two conversations. Typing is taken by the composer and never reaches here.
///
/// Returns `true` when the caller must stop the turn: Esc is the one action that changes what
/// the running turn is doing, and it has to be acted on by whoever owns the request.
fn on_turn_action(screen: &mut Screen, action: crate::ui::screen::Action) -> bool {
    use crate::ui::screen::Action;
    match action {
        Action::Line(text) => queue_mid_turn(screen, text, Vec::new()),
        Action::LineWithImages(text, images) => queue_mid_turn(screen, text, images),
        // Ctrl+O is exactly what a user does while a long tool call is on screen. The note
        // for "nothing to expand" comes from the screen itself.
        Action::ToggleExpand => {
            let _ = screen.toggle_last_collapsible();
        }
        Action::Stop => return true,
        // Ctrl+C / Ctrl+D during a turn do nothing: Ctrl+C clears the input line, and the
        // request itself is stopped with Esc, which is the key that says what it wants.
        Action::Interrupt | Action::Eof => {}
    }
    false
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
/// loop is async. Rather than colour everything async, the points where sync code has
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

/// Put one delta into the screen's buffer. No redraw: the caller batches.
fn apply_delta(screen: &mut Screen, delta: Delta) {
    match delta {
        Delta::Text(text) => screen.push_text(&text),
        Delta::Thinking(text) => screen.push_thinking(&text),
        Delta::Notice(text) => screen.push_lines(ui_compact::note_lines(
            &text,
            crate::ui::screen::Style::new(Color::Dim),
        )),
    }
}

/// Apply one delta, take everything queued behind it, and redraw **once**.
///
/// One redraw per burst, not one per token. A redraw erases the live region and paints it
/// again, so doing it per token repaints the input line — and the caret sitting in it — for
/// every character that arrives, which is what made the caret look unsteady while the model
/// thought. Exactly one render happens here even when the burst is a single token, or the
/// first character of an answer would sit in the buffer until the next event.
fn apply_deltas(
    screen: &mut Screen,
    first: Delta,
    deltas: &mut tokio::sync::mpsc::UnboundedReceiver<Delta>,
) {
    apply_delta(screen, first);
    while let Ok(delta) = deltas.try_recv() {
        apply_delta(screen, delta);
    }
    screen.render();
}

/// Move everything left in the channel into the screen, redrawing once if it was not empty.
///
/// The final drain after the stream ends: anything that arrived behind the last token still
/// has to be drawn, and there is no next event to draw it on.
fn drain_deltas(screen: &mut Screen, deltas: &mut tokio::sync::mpsc::UnboundedReceiver<Delta>) {
    let Some(first) = deltas.try_recv().ok() else {
        return;
    };
    apply_deltas(screen, first, deltas);
}

/// Exposed for tests: the dialect pi will hand to the policy.
pub fn dialect_for(config: &Config) -> Dialect {
    policy::configured_dialect(&config.shell.path)
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

mod commands;
mod handlers;
mod turn;

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
