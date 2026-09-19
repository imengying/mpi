//! mpi: a minimal terminal coding agent.
//!
//! The binary is a thin shell around [`agent::loop::Agent`]: read config, open (or resume) a
//! session, then alternate between reading a line and running a turn. Slash commands are
//! handled here so the loop itself stays about talking to the model.

use clap::Parser;

use mpi::agent::r#loop::{self, Agent};
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
        eprintln!("mpi: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // Update does not touch the config or the session store, so it runs before the
    // config load — `mpi update` has to work on a machine where the config is missing
    // or broken too.
    if cli.command == Some(Command::Update) {
        mpi::update::run()?;
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let config = match Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("mpi: {err}");
            eprintln!(
                "\n配置文件示例（{}）：\n{}",
                mpi::config::config_path().display(),
                SAMPLE_CONFIG
            );
            std::process::exit(1);
        }
    };

    if cli.command == Some(Command::Check) {
        print!("{}", r#loop::describe_environment(&cwd, &config));
        return Ok(());
    }

    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let mut agent = match &cli.command {
        Some(Command::Resume { id: Some(id) }) => {
            // An id that matches nothing is an error, not a silent fresh session: the user
            // asked for a specific conversation and getting a blank one would hide the
            // mistake until they noticed the missing history.
            match mpi::agent::session::find_by_prefix(id) {
                Ok(path) => Agent::resume(config, cwd, &path, interactive)?,
                Err(err) => anyhow::bail!("{err}（用 /resume 或 mpi resume 查看会话列表）"),
            }
        }
        Some(Command::Resume { id: None }) => match most_recent_session() {
            Some(path) => Agent::resume(config, cwd, &path, interactive)?,
            None => Agent::new(config, cwd, interactive)?,
        },
        _ => Agent::new(config, cwd, interactive)?,
    };

    // The turn loop: read a line, dispatch a slash command or run a turn.
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
                } else if let Err(err) = futures_block(agent.run_turn(line)) {
                    agent.screen.push_lines(ui_compact::note_lines(
                        &format!("本轮失败：{err:#}"),
                        mpi::ui::screen::Style::new(Color::Red),
                    ));
                }
            }
            Action::LineWithImages(line, images) => {
                let line = line.trim().to_string();
                // Slash commands are text-only: a command with an image attached is not
                // something mpi defines, so the images are dropped rather than silently
                // sent as a turn.
                if line.starts_with('/') {
                    if !futures_block(agent.command(&line))? {
                        break;
                    }
                } else if let Err(err) = futures_block(agent.run_turn_with_images(&line, images)) {
                    agent.screen.push_lines(ui_compact::note_lines(
                        &format!("本轮失败：{err:#}"),
                        mpi::ui::screen::Style::new(Color::Red),
                    ));
                }
            }
            Action::ToggleExpand => {
                if !agent.screen.toggle_last_collapsible() {
                    agent.screen.push_lines(ui_compact::note_lines(
                        "没有可展开的内容。",
                        mpi::ui::screen::Style::new(Color::Dim),
                    ));
                }
            }
            Action::Interrupt => continue,
            Action::Eof => break,
        }
    }

    // Leave the user something they can act on: the exact command that brings this
    // conversation back, not the path of the file it happens to live in. The id is the
    // file name, so it is already the shortest thing that names the session.
    //
    // After `/delete` there is no session to resume, so nothing is printed — offering a
    // command for a file that no longer exists would be worse than silence.
    if !agent.session_deleted() {
        let id = agent.session.id();
        agent.screen.push_lines(ui_compact::note_lines(
            &format!("继续此会话：mpi resume {id}"),
            mpi::ui::screen::Style::new(Color::Dim),
        ));
    }
    agent.render_footer(None);
    Ok(())
}

fn most_recent_session() -> Option<std::path::PathBuf> {
    mpi::agent::session::list().first().map(|summary| summary.path.clone())
}

/// Drive an async agent call from the synchronous turn loop.
fn futures_block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a Tokio runtime")
        .block_on(future)
}

/// Printed when the config is missing or unreadable. There is no built-in model list: the
/// user has to say which models exist, and this is the shape to say it in.
const SAMPLE_CONFIG: &str = r#"{
  "shell": { "path": "/usr/bin/zsh" },
  "providers": [
    {
      "name": "name",
      "api": "openai-completions",
      "base_url": "url",
      "api_key_env": "NAME_API_KEY",
      "models": [
        {
          "id": "deepseek-v4.1-flash",
          "name": "deepseek-v4.1-flash",
          "context_window": 1000000,
          "max_tokens": 64000,
          "reasoning": true,
          "thinking_levels": ["low", "high", "max"]
        }
      ]
    }
  ],
  "default_model": "name/deepseek-v4.1-flash"
}"#;
