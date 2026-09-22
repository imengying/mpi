//! pi: a minimal terminal coding agent.
//!
//! The binary is a thin shell around [`agent::loop::Agent`]: read config, open (or resume) a
//! session, then alternate between reading a line and running a turn. Slash commands are
//! handled here so the loop itself stays about talking to the model.

use clap::Parser;

use mpi::agent::r#loop::Agent;
use mpi::cli::{Cli, Command};
use mpi::config::Config;
use mpi::ui::screen::{teardown, Action};
use mpi::ui::{compact as ui_compact, theme::Color};

fn main() {
    let cli = Cli::parse();
    let result = run(cli);
    // Leave the terminal as it was found, window title included.
    teardown();
    mpi::ui::screen::clear_title();
    if let Err(err) = result {
        eprintln!("pi: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Update does not touch the config or the session store, so it runs before the
    // config load — `pi update` has to work on a machine where the config is missing
    // or broken too.
    if cli.command == Some(Command::Update) {
        mpi::update::run()?;
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
    // A turn can be handed more work while it runs — the user types into the input line and
    // Enter queues the message — so each turn is followed by whatever was queued behind it,
    // in the order it was typed. That is a loop rather than an `if`, because answering one
    // queued message can take long enough to collect another.
    loop {
        match agent.read_input() {
            Action::Line(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with('/') {
                    if !futures_block(agent.command(line))? {
                        break;
                    }
                } else if !run_turn_and_drain(&mut agent, line, Vec::new())? {
                    break;
                }
            }
            Action::LineWithImages(line, images) => {
                let line = line.trim().to_string();
                // Slash commands are text-only: a command with an image attached is not
                // something pi defines, so the images are dropped rather than silently
                // sent as a turn.
                if line.starts_with('/') {
                    if !futures_block(agent.command(&line))? {
                        break;
                    }
                } else if !run_turn_and_drain(&mut agent, &line, images)? {
                    break;
                }
            }
            Action::ToggleExpand => {
                // The note for "nothing to expand" comes from the screen itself.
                let _ = agent.screen.toggle_last_collapsible();
            }
            Action::Interrupt => continue,
            // Esc stops the turn in flight, and this is the prompt: nothing is running, so
            // there is nothing to stop. The screen only raises it while the spinner is up,
            // which is why reaching here is a no-op rather than a special case.
            Action::Stop => continue,
            Action::Eof => break,
        }
    }

    // Leave the user something they can act on: the exact command that brings this
    // conversation back, not the path of the file it happens to live in. The id is the
    // file name, so it is already the shortest thing that names the session.
    //
    // After `/delete` there is no session to resume, so nothing is printed — offering a
    // command for a file that no longer exists would be worse than silence.
    // A session that never said anything has no file, so there is no command to give: the
    // id would name nothing, and printing one anyway would send the user to an error.
    if !agent.session_deleted() && agent.session().is_saved() {
        let id = agent.session().id();
        agent.screen.push_lines(ui_compact::note_lines(
            &format!("继续此会话：pi resume {id}"),
            mpi::ui::screen::Style::new(Color::Dim),
        ));
    }
    // Take the live region down before returning: it is the input prompt and footer, not
    // part of the transcript, and leaving it behind hands the shell a cursor parked mid-row.
    agent.screen.leave();
    Ok(())
}

/// Run one turn, then act on everything that was queued behind it.
///
/// The queue comes from the input line staying live during the turn: Enter there could not
/// start a second turn underneath the one in flight, so the line was held instead. A queued
/// message becomes a turn of its own; a queued command runs; both in the order they were
/// typed, because that is the order the user wrote them in. The loop ends when a turn
/// finishes with an empty queue — an answer can take long enough to collect more while it
/// runs.
fn run_turn_and_drain(
    agent: &mut Agent,
    line: &str,
    images: Vec<mpi::image_input::PastedImage>,
) -> anyhow::Result<bool> {
    use mpi::ui::screen::Queued;
    let mut queue: std::collections::VecDeque<Queued> = [Queued::Message(line.to_string(), images)]
        .into();
    while let Some(item) = queue.pop_front() {
        match item {
            Queued::Command(command) => {
                // A command can end the session (`/exit`, `/delete`), and that decision was
                // made by the user before this turn even finished — it is carried out as
                // given rather than being dropped on the floor.
                if !futures_block(agent.command(&command))? {
                    return Ok(false);
                }
            }
            Queued::Message(text, images) => {
                if let Err(err) = futures_block(agent.run_turn_with_images(&text, images)) {
                    // A failed turn is reported and the session carries on: the conversation
                    // is still perfectly usable, and ending the program over one bad request
                    // would throw it away. Whatever was queued behind it is still acted on,
                    // because it was typed and the user is waiting to see it run.
                    agent.screen.push_lines(ui_compact::note_lines(
                        &format!("本轮失败：{err:#}"),
                        mpi::ui::screen::Style::new(Color::Red),
                    ));
                }
            }
        }
        queue.extend(agent.screen.take_queued());
    }
    Ok(true)
}

fn most_recent_session(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    mpi::agent::session::list(cwd).first().map(|summary| summary.path.clone())
}

/// Drive an async agent call from the synchronous turn loop.
fn futures_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a Tokio runtime")
        .block_on(future)
}

