//! pi: a minimal terminal coding agent.
//!
//! The binary is a thin shell around `mpi::agent::loop::Agent`: read config, open (or resume) a
//! session, then alternate between reading input and driving the agent on one runtime.

use anyhow::Context;
use clap::Parser;

use mpi::agent::r#loop::Agent;
use mpi::cli::{Cli, Command};
use mpi::config::Config;
use mpi::ui::compact as ui_compact;
use mpi::ui::screen::{Action, teardown};

fn main() {
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        teardown();
        mpi::ui::screen::clear_title();
        panic_hook(info);
    }));
    let cli = Cli::parse();
    let result = run(cli);
    // Leave the terminal as it was found, window title included.
    teardown();
    mpi::ui::screen::clear_title();
    if let Some(signal) = mpi::ui::terminal::exit_signal() {
        std::process::exit(128 + signal);
    }
    if let Err(err) = result {
        eprintln!("pi: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("构建 Tokio 运行时失败")?;
    runtime.block_on(async {
        #[cfg(unix)]
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            // The watcher also wakes synchronous input loops. Dropping the session
            // future cancels its tools before the runtime and terminal are torn down.
            let mut shutdown =
                mpi::ui::terminal::watch_exit_signals().context("监听退出信号失败")?;
            let result = tokio::select! {
                biased;
                _ = &mut shutdown => Ok(()),
                result = run_session(cli) => result,
            };
            shutdown.abort();
            return result;
        }
        run_session(cli).await
    })
}

async fn run_session(cli: Cli) -> anyhow::Result<()> {
    // Update does not touch the config or the session store, so it runs before the
    // config load — `pi update` has to work on a machine where the config is missing
    // or broken too.
    if cli.command == Some(Command::Update) {
        mpi::update::run().await?;
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            // The messages that already carry the path and the next step are printed as-is:
            // prefixing them with `pi:` would say the same thing twice, and repeating the
            // missing-provider advice in a second line is the same mistake in another shape.
            match &err {
                mpi::config::ConfigError::Created(_) => eprintln!("{err}"),
                _ => eprintln!("pi: {err}"),
            }
            std::process::exit(1);
        }
    };

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut agent = match &cli.command {
        Some(Command::Resume { id: Some(id) }) => {
            // An id that matches nothing is an error, not a silent fresh session: the user
            // asked for a specific conversation and getting a blank one would hide the
            // mistake until they noticed the missing history.
            match mpi::agent::session::find_by_prefix(id, &cwd) {
                Ok(path) => Agent::resume(config, cwd, &path, interactive)?,
                Err(err) => anyhow::bail!("{err}（用 /resume 或 pi resume 查看会话列表）"),
            }
        }
        Some(Command::Resume { id: None }) => match most_recent_session(&cwd) {
            Some(path) => Agent::resume(config, cwd, &path, interactive)?,
            None => Agent::new(config, cwd, interactive)?,
        },
        _ => Agent::new(config, cwd, interactive)?,
    };

    // The turn loop: read a line, dispatch a slash command or run a turn.
    //
    // Messages steer the active turn at safe boundaries; slash commands dispatch immediately.
    loop {
        match agent.read_input() {
            Action::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with('/') {
                    if !agent.command(line).await? {
                        break;
                    }
                } else if !run_turn_and_drain(&mut agent, line, Vec::new()).await? {
                    break;
                }
            }
            Action::LineWithImages(line, images) => {
                let line = line.trim().to_string();
                // Slash commands are text-only: a command with an image attached is not
                // something pi defines, so the images are dropped rather than silently
                // sent as a turn.
                if line.starts_with('/') {
                    if !agent.command(&line).await? {
                        break;
                    }
                } else if !run_turn_and_drain(&mut agent, &line, images).await? {
                    break;
                }
            }
            Action::ToolDetails => {
                // The note for "nothing to expand" comes from the screen itself.
                let _ = agent.screen.show_tool_details();
            }
            Action::Selection(index) => agent.handle_selection(index)?,
            Action::Interrupt => break,
            // Esc stops the turn in flight, and this is the prompt: nothing is running, so
            // there is nothing to stop. The screen only raises it while the spinner is up,
            // which is why reaching here is a no-op rather than a special case.
            Action::Stop => continue,
            Action::Eof => break,
        }
    }

    // Piped transcripts retain a resume hint. Interactive sessions restore the shell.
    if !agent.screen.interactive() && !agent.session_deleted() && agent.session().is_saved() {
        let id = agent.session().id();
        agent
            .screen
            .push_lines(ui_compact::note(&format!("继续此会话：pi resume {id}")));
    }
    // Restore the terminal before returning, including when a detail view is open.
    agent.screen.leave();
    Ok(())
}

/// Run a turn and drain ordinary messages; slash commands are handled immediately.
async fn run_turn_and_drain(
    agent: &mut Agent,
    line: &str,
    images: Vec<mpi::image_input::PastedImage>,
) -> anyhow::Result<bool> {
    use mpi::ui::screen::Queued;
    let mut next = Some(Queued::new(line.to_string(), images));
    while let Some(item) = next {
        if let Err(err) = agent.run_turn_with_images(&item.text, item.images).await {
            // A failed turn is reported and the session carries on: the conversation
            // is still perfectly usable, and ending the program over one bad request
            // would throw it away. Whatever was queued behind it is still acted on,
            // because it was typed and the user is waiting to see it run.
            agent
                .screen
                .push_lines(ui_compact::error(&format!("本轮失败：{err:#}")));
        }
        if agent.wants_exit() {
            return Ok(false);
        }
        next = agent.screen.pop_queued();
    }
    Ok(true)
}

fn most_recent_session(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    mpi::agent::session::list(cwd)
        .first()
        .map(|summary| summary.path.clone())
}
