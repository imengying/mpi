use clap::Parser;

/// The version reported by `--version`.
///
/// The release workflow passes the pushed tag in as `MPI_BUILD_VERSION`, so a tagged build
/// reports exactly the tag. A local build falls back to the crate version.
pub fn version() -> &'static str {
    option_env!("MPI_BUILD_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

#[derive(Debug, Parser)]
#[command(name = "mpi", version = version(), about = "极简终端 AI 编程代理")]
pub struct Cli {
    /// 继续最近一次会话
    #[arg(short, long)]
    pub resume: bool,
    /// 只读检查配置后退出
    #[arg(long)]
    pub check: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        // A tagged build overrides this; either way it has to say something.
        assert!(!version().trim().is_empty());
    }
}
