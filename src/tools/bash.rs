//! `bash`: run a command through zsh.
//!
//! The shell is fixed to the configured path (default `/usr/bin/zsh`) and is invoked as
//! `zsh -c '<command>'`. The command string handed here is the one the authorization
//! gate already vetted — for auto-approved calls it is the rewritten form with quoted
//! absolute paths, so a PATH change cannot redirect it.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncReadExt;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "在 zsh 中执行一条命令并返回输出。只读的简单命令会自动执行，\
                      其余命令会在执行前请求用户授权。命令输出默认只显示末尾若干行。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令（zsh 语法）" },
                "description": { "type": "string", "description": "一句话说明这条命令做什么" }
            },
            "required": ["command"]
        }),
    }
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let command = crate::tools::required_str(arguments, "command")?;
    execute_with_shell(command, cwd, &crate::config::DEFAULT_SHELL).await
}

/// Run `command` in `shell`, with the working directory set to `cwd`.
pub async fn execute_with_shell(
    command: &str,
    cwd: &Path,
    shell: &str,
) -> Result<ToolOutput, String> {
    let shell = if shell.is_empty() { crate::config::DEFAULT_SHELL } else { shell };
    let mut child = tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("无法启动 {shell}：{err}"))?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout).await;
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr).await;
    }
    let status = child
        .wait()
        .await
        .map_err(|err| format!("等待命令结束失败：{err}"))?;
    let code = status.code();

    let mut body = crate::util::sanitize(&stdout);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&crate::util::sanitize(&stderr));
    }
    let body = body.trim_end_matches('\n').to_string();

    let failed = code != Some(0);
    let mut content = String::new();
    if !body.is_empty() {
        content.push_str(&body);
        content.push('\n');
    }
    match code {
        Some(0) => {}
        Some(code) => content.push_str(&format!("[退出码 {code}]\n")),
        None => content.push_str("[命令被信号终止]\n"),
    }
    let mut footer: Vec<String> = Vec::new();
    if failed {
        footer.push(match code {
            Some(0) | None => "命令被信号终止".to_string(),
            Some(code) => format!("退出码 {code}"),
        });
    }
    Ok(ToolOutput {
        content,
        display: Display::Command { expanded: false, footer },
        is_error: failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    

    #[test]
    fn runs_through_zsh_and_reports_the_exit_code() {
        let cwd = std::env::temp_dir();
        let out = block(execute_with_shell("echo hi", &cwd, "/usr/bin/zsh")).unwrap();
        assert!(out.content.contains("hi"));
        assert!(!out.is_error);

        let failed = block(execute_with_shell("echo oops; exit 3", &cwd, "/usr/bin/zsh")).unwrap();
        assert!(failed.is_error);
        assert!(failed.content.contains("[退出码 3]"));
    }

    #[test]
    fn zsh_specific_syntax_works() {
        let cwd = std::env::temp_dir();
        // `$^array` style expansion only exists in zsh; this proves the shell really is zsh.
        let out = block(execute_with_shell("echo ${(U)foo}", &cwd, "/usr/bin/zsh")).unwrap();
        assert!(!out.content.contains("bad substitution"), "{}", out.content);
    }

    #[test]
    fn stderr_is_captured_and_ansi_is_removed() {
        let cwd = std::env::temp_dir();
        let out = block(execute_with_shell(
            "printf '\\033[31mred\\033[0m\\n' 1>&2",
            &cwd,
            "/usr/bin/zsh",
        ))
        .unwrap();
        assert!(out.content.contains("red"));
        assert!(!out.content.contains('\u{1b}'));
    }
}
