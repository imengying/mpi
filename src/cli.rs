use clap::{Parser, Subcommand};

/// The version reported by `--version`.
///
/// The release workflow passes the pushed tag in as `PI_BUILD_VERSION` (already stripped
/// of its leading `v` by `build.rs`), so a tagged build reports exactly the tag. A local
/// build falls back to the crate version.
pub fn version() -> &'static str {
    option_env!("PI_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// 继续最近一次会话；带 id 前缀时恢复指定的那个
    Resume {
        /// 会话 id（或其前缀）。省略则用最近一次。
        id: Option<String>,
    },
    /// 更新到最新 Release
    Update,
}

#[derive(Debug, Parser)]
#[command(name = "pi", version = version(), about = "极简终端 AI 编程代理")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        // A tagged build overrides this; either way it has to say something.
        assert!(!version().trim().is_empty());
    }

    #[test]
    fn the_cli_parses_codex_style_subcommands() {
        use clap::Parser as _;
        for (arg, expected) in [("resume", "Resume"), ("update", "Update")] {
            let cli = Cli::try_parse_from(["pi", arg]).unwrap();
            // `resume` carries an optional id, so match on the variant name.
            let actual = format!("{:?}", cli.command);
            assert!(actual.starts_with(&format!("Some({expected}")), "{actual}");
        }
        // The id is optional, and a prefix is enough to name a session.
        let with_id = Cli::try_parse_from(["pi", "resume", "01a0b8d0"]).unwrap();
        assert_eq!(
            with_id.command,
            Some(Command::Resume { id: Some("01a0b8d0".to_string()) })
        );
        let without = Cli::try_parse_from(["pi", "resume"]).unwrap();
        assert_eq!(without.command, Some(Command::Resume { id: None }));
        let plain = Cli::try_parse_from(["pi"]).unwrap();
        assert!(plain.command.is_none());
    }
}
