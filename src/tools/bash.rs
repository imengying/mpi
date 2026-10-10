//! `bash`: run a command through zsh.
//!
//! The shell is fixed to the configured path (default `/usr/bin/zsh`) and is invoked as
//! `zsh -c '<command>'`. The command string handed here is the one the authorization
//! gate already vetted — for auto-approved calls it is the rewritten form with quoted
//! absolute paths, so a PATH change cannot redirect it.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "在 zsh 中执行一条命令并返回输出。只读的简单命令会自动执行，\
                      其余命令会在执行前请求用户授权。自动授权的内容搜索会跳过敏感文件。\
                      命令输出默认只显示末尾若干行。\
                      列目录用 ls 工具、按内容搜索用 grep 工具、按文件名查找用 find 工具：\
                      它们不需要引号转义，也不会因为 zsh 的通配符匹配不到而失败。\
                      要在别的目录里执行，用 cwd 参数，不要写 `cd X && ...`。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令（zsh 语法）" },
                "description": { "type": "string", "description": "一句话说明这条命令做什么" },
                "cwd": { "type": "string", "description": "命令的工作目录，默认当前工作目录" }
            },
            "required": ["command"]
        }),
    }
}

/// Where a `bash` call runs, from its own `cwd` argument.
///
/// 89% of the calls in the recorded sessions opened with `cd X && ...` — 1738 of 1952, and
/// 1711 of those never changed directory again. The prefix is pure overhead: it is re-typed
/// every turn, it makes every command read as if it moved somewhere, and it costs a policy
/// decision on each turn because `cd` is exactly what the command checker has to reason
/// about. A parameter says the same thing once and keeps the command itself about the work.
///
/// Resolved through the same path helper the file tools use, so `~`, relative names and
/// `.` mean here what they mean everywhere else in the tool set.
pub fn resolve_cwd(arguments: &serde_json::Value, cwd: &Path) -> Result<PathBuf, String> {
    let Some(value) = arguments.get("cwd") else {
        return Ok(cwd.to_path_buf());
    };
    let raw = value
        .as_str()
        .ok_or_else(|| "cwd 必须是字符串".to_string())?
        .trim();
    // An empty value means "not given" rather than "the filesystem root".
    if raw.is_empty() {
        return Ok(cwd.to_path_buf());
    }
    let resolved = crate::auth::policy::resolve_tool_path(raw, cwd);
    if !resolved.is_dir() {
        return Err(format!("cwd 不是目录：{raw}"));
    }
    Ok(resolved)
}

/// Run `command` in `shell`, with the working directory set to `cwd`.
pub async fn execute_with_shell(
    command: &str,
    cwd: &Path,
    shell: &str,
) -> Result<ToolOutput, String> {
    let shell = if shell.is_empty() {
        crate::config::DEFAULT_SHELL
    } else {
        shell
    };
    let capture =
        super::output::Capture::new().map_err(|err| format!("创建输出文件失败：{err}"))?;
    let stdout = capture.file.try_clone().map_err(|err| err.to_string())?;
    let stderr = capture.file.try_clone().map_err(|err| err.to_string())?;
    let mut process = tokio::process::Command::new(shell);
    process
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = super::process::Child::spawn(&mut process)
        .map_err(|err| format!("无法启动 {shell}：{err}"))?;

    let status = child
        .wait()
        .await
        .map_err(|err| format!("等待命令结束失败：{err}"))?;
    let code = status.code();

    let (body, full_path) = capture
        .finish(true)
        .map_err(|err| format!("读取命令输出失败：{err}"))?;

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
    if let Some(path) = full_path {
        footer.push(format!("完整输出：{}", path.display()));
    }
    if failed {
        footer.push(match code {
            Some(0) | None => "命令被信号终止".to_string(),
            Some(code) => format!("退出码 {code}"),
        });
    }
    Ok(ToolOutput {
        content,
        display: Display::Command { footer },
        is_error: failed,
        duration: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;

    #[tokio::test]
    async fn cancellation_stops_background_descendants() {
        let dir = std::env::temp_dir().join(format!("pi-cancel-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let task_dir = dir.clone();
        let task = tokio::spawn(async move {
            execute_with_shell(
                "sh -c 'printf ready > ready; sleep 0.5; printf survived > marker' & wait",
                &task_dir,
                "/usr/bin/zsh",
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(
            !dir.join("marker").exists(),
            "cancelled grandchildren must not continue writing"
        );
        execute_with_shell(
            "sh -c 'sleep 0.1; printf complete > normal' &",
            &dir,
            "/usr/bin/zsh",
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.join("normal").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("normal completion must preserve an explicitly approved background job");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_stderr_cannot_block_stdout_and_keeps_the_full_log() {
        let out = block(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                execute_with_shell(
                    "printf '%*s' 262144 '' >&2; printf '\\nlast-output\\n'",
                    &std::env::temp_dir(),
                    "/usr/bin/zsh",
                ),
            )
            .await
            .expect("stderr must not deadlock the command")
            .unwrap()
        });
        assert!(out.content.contains("last-output"));
        assert!(out.content.len() <= crate::util::MAX_OUTPUT_BYTES);
        let Display::Command { footer, .. } = out.display else {
            panic!("command display")
        };
        let path = footer
            .iter()
            .find_map(|line| line.strip_prefix("完整输出："))
            .unwrap();
        assert!(std::fs::metadata(path).unwrap().len() > 262144);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn non_utf8_output_does_not_discard_the_rest_of_the_stream() {
        let out = block(execute_with_shell(
            "printf '\\377ok\\n'",
            &std::env::temp_dir(),
            "/usr/bin/zsh",
        ))
        .unwrap();
        assert!(out.content.contains("ok"));
    }

    #[test]
    fn runs_through_zsh_and_reports_the_exit_code() {
        let cwd = std::env::temp_dir();
        let out = block(execute_with_shell("echo hi", &cwd, "/usr/bin/zsh")).unwrap();
        assert!(out.content.contains("hi"));
        assert!(!out.is_error);

        let failed = block(execute_with_shell(
            "echo oops; exit 3",
            &cwd,
            "/usr/bin/zsh",
        ))
        .unwrap();
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
