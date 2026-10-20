//! `bash`: run a command through zsh.
//!
//! The shell is fixed to the configured path (default `/usr/bin/zsh`) and is invoked as
//! `zsh -o no_nomatch -o noequals -c '<command>'`. The command string handed here is the
//! one the authorization gate already vetted — for auto-approved calls it is the rewritten
//! form with quoted absolute paths, so a PATH change cannot redirect it.

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
                      它们不需要引号转义，也不会因为 zsh 的通配符匹配不到而失败。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令。不要用 `cd X && ...` 换目录，用 cwd 参数" },
                "description": { "type": "string", "description": "一句话说明这条命令做什么" },
                "cwd": { "type": "string", "description": "命令的工作目录。默认是当前工作目录；要在别处执行就用这个参数，命令里不要再写 cd" }
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

/// The options a zsh call needs to behave the way the caller expects.
///
/// Both of these exist because zsh is stricter than the shell most callers have in mind, and
/// the strictness lands as a *failed command*, not as a difference in output. Neither is a
/// convenience: without them the command does not run at all.
///
/// `no_nomatch`: an unmatched glob is an error in zsh, and the error aborts the whole command
/// before any of it runs. `grep -rn pattern src --include=*.rs` — which works in bash, and is
/// how the option is habitually written — died with `no matches found` and searched nothing.
/// In the recorded sessions that was 61 calls. With the option off, the pattern reaches the
/// command unchanged, which is what every other shell does.
///
/// `noequals`: `echo ===` is a separator, not an equals expansion. zsh reads `=cmd` as "the
/// path to cmd" (a shortcut for `command -v`), so a run of `=` signs becomes a lookup for a
/// program named `=` and the line fails with `== not found`. 31 calls. Confirmed harmless to
/// the things that look similar: `[[ x =~ y ]]`, `[ x = y ]`, `FOO=bar` and `--opt=value` all
/// still work, and the recorded sessions never once relied on `=cmd` expansion.
///
/// Only applied to zsh — `-o no_nomatch` on bash is an error, not an ignored flag.
fn shell_options(shell: &str) -> Vec<&'static str> {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name.eq_ignore_ascii_case("zsh") {
        vec!["-o", "no_nomatch", "-o", "noequals"]
    } else {
        Vec::new()
    }
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
    process.args(shell_options(shell));
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
    Ok(crate::tools::tool_output(
        content,
        Display::Command { footer },
        failed,
    ))
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
    fn an_unmatched_glob_reaches_the_command_instead_of_aborting_it() {
        // The shape of 61 recorded failures. `--include=*.rs` is how the option is written in
        // bash, and zsh's default is to fail the whole command on the unmatched glob before
        // grep ever runs — so the search silently did not happen.
        let dir = std::env::temp_dir().join(format!("pi-nomatch-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("hit.rs"), "needle-in-glob\n").unwrap();
        let out = block(execute_with_shell(
            "grep -rn needle-in-glob . --include=*.rs",
            &dir,
            "/usr/bin/zsh",
        ))
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("needle-in-glob"), "{}", out.content);

        // A glob that matches nothing is passed through as a literal, as in bash, rather than
        // killing the command.
        let literal = block(execute_with_shell(
            "printf '%s' *.nomatch",
            &dir,
            "/usr/bin/zsh",
        ))
        .unwrap();
        assert!(literal.content.contains("*.nomatch"), "{}", literal.content);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_run_of_equals_signs_is_an_argument_not_an_equals_expansion() {
        // The other recorded failure: `echo ===` as a separator. zsh expands `=cmd` to the
        // path of cmd, so `===` became a lookup for a program named `=` and the line failed.
        let cwd = std::env::temp_dir();
        let out = block(execute_with_shell("echo ===", &cwd, "/usr/bin/zsh")).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("==="), "{}", out.content);

        // The neighbouring syntax that must keep working.
        for (command, marker) in [
            ("[ a = a ] && echo same", "same"),
            ("[[ abc =~ ^a ]] && echo regex", "regex"),
            ("FOO=bar; echo \"set:$FOO\"", "set:bar"),
        ] {
            let out = block(execute_with_shell(command, &cwd, "/usr/bin/zsh")).unwrap();
            assert!(
                !out.is_error && out.content.contains(marker),
                "{command}: {}",
                out.content
            );
        }
    }

    #[test]
    fn the_options_are_only_handed_to_zsh() {
        // `-o no_nomatch` is not a bash flag: passing it there would be an error, not a
        // no-op, so the guard has to be by shell name.
        assert_eq!(
            shell_options("/usr/bin/zsh"),
            vec!["-o", "no_nomatch", "-o", "noequals"]
        );
        assert_eq!(
            shell_options("zsh"),
            vec!["-o", "no_nomatch", "-o", "noequals"]
        );
        assert_eq!(
            shell_options("/usr/bin/zsh.exe"),
            vec!["-o", "no_nomatch", "-o", "noequals"]
        );
        assert!(shell_options("/usr/bin/bash").is_empty());
        assert!(shell_options("/bin/sh").is_empty());
        assert!(shell_options("").is_empty());
    }

    #[test]
    fn a_non_zsh_shell_still_runs_the_command() {
        // The options are skipped for other shells, and the command must still work there.
        let cwd = std::env::temp_dir();
        for shell in ["/bin/sh", "/usr/bin/bash"] {
            let out = block(execute_with_shell("echo plain", &cwd, shell)).unwrap();
            assert!(!out.is_error, "{shell}: {}", out.content);
            assert!(out.content.contains("plain"), "{shell}: {}", out.content);
        }
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
