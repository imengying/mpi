//! The authorization policy.
//!
//! Two kinds of judgement live here, and they are kept apart because they answer different
//! questions and change for different reasons:
//!
//! * `path` — "may this path be touched?" A path is judged by where it resolves to, so
//!   this is about symlinks, the home directory, and the names that hold secrets.
//! * `command` — "may this command line run?" A command is judged by parsing a literal
//!   shell subset and checking every word, so this is about option syntax, quoting and the
//!   shell dialect.
//!
//! What they share — the [`Assessment`] a check returns, the [`Dialect`] it is judged under,
//! and the [`Operation`] being attempted — is defined here, next to [`assess_tool`], which is
//! the entry point that dispatches a tool call to one half or the other.
//!
//! Both halves are refusal-first: anything not understood is sent to the user rather than
//! guessed at. A command pi cannot read is a command it asks about.

use std::path::Path;

use crate::config::DEFAULT_SHELL;

/// Ask the user before running. `ask` carries the reason handed to the model on refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Allow { safe_command: Option<String> },
    Ask { reason: String },
}

impl Assessment {
    pub fn ask(reason: impl Into<String>) -> Self {
        Assessment::Ask {
            reason: reason.into(),
        }
    }

    pub fn allow() -> Self {
        Assessment::Allow { safe_command: None }
    }

    pub fn allows(&self) -> bool {
        matches!(self, Assessment::Allow { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Assessment::Ask { reason } => Some(reason),
            _ => None,
        }
    }
}

/// The refusal text the model receives. Without the reason the model can only guess
/// why it was blocked and re-issues the same call.
pub fn refusal(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("未获得用户授权，操作未执行（{reason}）"),
        None => "未获得用户授权，操作未执行".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    Zsh,
}

impl Dialect {
    /// Select the expansion rules of the configured shell.
    pub fn for_shell_path(path: &str) -> Dialect {
        let name = path.rsplit('/').next().unwrap_or(path);
        let name = name.strip_suffix(".exe").unwrap_or(name);
        if name.eq_ignore_ascii_case("zsh") {
            Dialect::Zsh
        } else {
            Dialect::Bash
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Dialect::Zsh => "zsh",
            Dialect::Bash => "bash",
        }
    }
}

/// The dialect pi will actually use, derived from the configured shell.
pub fn configured_dialect(shell_path: &str) -> Dialect {
    Dialect::for_shell_path(if shell_path.is_empty() {
        DEFAULT_SHELL
    } else {
        shell_path
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
}

mod command;
mod path;

pub use command::assess_command;
pub(crate) use path::search_exclusions;
pub use path::{assess_path, resolve_tool_path};

/// Tools that only ever read.
pub fn assess_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    dialect: Dialect,
) -> Assessment {
    let string = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "bash" => assess_command(string("command"), cwd, dialect),
        "write" | "edit" => {
            let path = string("path");
            assess_path(Operation::Write, path, cwd)
        }
        "read" | "grep" | "find" | "ls" => {
            let path = {
                let explicit = string("path");
                if explicit.is_empty() {
                    cwd.to_string_lossy().to_string()
                } else {
                    explicit.to_string()
                }
            };
            assess_path(Operation::Read, &path, cwd)
        }
        _ => Assessment::ask("自定义工具尚未归类，需要确认其操作"),
    }
}

#[cfg(test)]
mod tests;
