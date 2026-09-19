use clap::{Parser, Subcommand};

/// The version reported by `--version`.
///
/// The release workflow passes the pushed tag in as `MPI_BUILD_VERSION` (already stripped
/// of its leading `v` by `build.rs`), so a tagged build reports exactly the tag. A local
/// build falls back to the crate version.
pub fn version() -> &'static str {
    option_env!("MPI_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// 继续最近一次会话
    Resume,
    /// 只检查配置与环境，不进交互
    Check,
    /// 更新到最新 Release
    Update,
}

#[derive(Debug, Parser)]
#[command(name = "mpi", version = version(), about = "极简终端 AI 编程代理")]
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
        for (arg, expected) in [("resume", "Resume"), ("check", "Check"), ("update", "Update")] {
            let cli = Cli::try_parse_from(["mpi", arg]).unwrap();
            assert_eq!(format!("{:?}", cli.command), format!("Some({expected})"));
        }
        let plain = Cli::try_parse_from(["mpi"]).unwrap();
        assert!(plain.command.is_none());
    }
}
