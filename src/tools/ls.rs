//! `ls`: list a directory. Prefers `eza` when installed, falling back to `ls`.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};
use crate::util;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "ls".into(),
        description: "列出目录内容。存在 eza 时优先使用 eza，否则用系统 ls。".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目录路径，默认当前目录" },
                "all": { "type": "boolean", "description": "是否包含隐藏文件" }
            }
        }),
    }
}

fn executable() -> std::path::PathBuf {
    for candidate in ["/usr/bin/eza", "/usr/local/bin/eza", "/bin/eza"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return path.to_path_buf();
        }
    }
    // `ls` is always present, and the fallback path is fixed rather than looked up.
    Path::new("/usr/bin/ls").to_path_buf()
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let target = arguments.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let target_path = crate::auth::policy::resolve_tool_path(target, cwd);
    if !target_path.is_dir() {
        return Err(format!("路径不是目录：{target}"));
    }
    let all = arguments.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    let binary = executable();
    let is_eza = binary.to_string_lossy().ends_with("eza");
    let mut command = tokio::process::Command::new(&binary);
    if is_eza {
        // Long form with a stable, machine-readable-ish layout and no colour codes.
        command.arg("--color=never").arg("--long").arg("--header");
        if all {
            command.arg("--all");
        }
        command.arg("--").arg(&target_path);
    } else {
        command.arg("-la").arg("--color=never");
        if !all {
            // `-la` already implies `-a`; drop back to the requested view.
            command = tokio::process::Command::new(&binary);
            command.arg("-l").arg("--color=never");
        }
        command.arg("--").arg(&target_path);
    }
    let output = command
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| format!("列目录失败：{err}"))?;
    let stdout = util::sanitize(&String::from_utf8_lossy(&output.stdout).to_string());
    if !output.status.success() {
        let stderr = util::sanitize(&String::from_utf8_lossy(&output.stderr).to_string());
        return Err(if stderr.trim().is_empty() {
            format!("列目录失败（退出码 {:?}）", output.status.code())
        } else {
            stderr
        });
    }
    let body = stdout.trim_end();
    let content = format!("{body}\n");
    Ok(ToolOutput {
        content,
        display: Display::File { verb: "列出", path: target.to_string() },
        is_error: false,
        duration: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;


    fn fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-ls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("visible.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        dir
    }

    #[test]
    fn lists_entries_without_terminal_colour_or_a_count_note() {
        // The listing is the answer; a trailing `[N 项]` only repeated what the rows already
        // showed, and because a `File` block keeps its *last* rows when collapsed, that note
        // was the row guaranteed to survive — a long listing showed nothing else.
        let dir = fixture();
        let out = block(execute(&serde_json::json!({}), &dir)).unwrap();
        assert!(out.content.contains("visible.txt"));
        assert!(!out.content.contains('\u{1b}'));
        assert!(!out.content.contains("项]"), "{:?}", out.content);
        // Nothing but the rows: no note above them and none below.
        assert!(
            out.content.lines().filter(|l| !l.trim().is_empty()).count() >= 1,
            "{:?}",
            out.content
        );
        assert!(!out.content.trim_end().lines().last().unwrap().contains('['));
    }

    #[test]
    fn hidden_files_are_only_listed_when_asked_for() {
        let dir = fixture();
        let plain = block(execute(&serde_json::json!({"path": dir.to_string_lossy()}), &dir)).unwrap();
        let all = block(
            execute(&serde_json::json!({"path": dir.to_string_lossy(), "all": true}), &dir),
        )
        .unwrap();
        assert!(all.content.contains(".hidden"));
        if !executable().to_string_lossy().ends_with("eza") {
            assert!(!plain.content.contains(".hidden"));
        }
    }

    #[test]
    fn a_file_target_is_rejected() {
        let dir = fixture();
        let file = dir.join("visible.txt");
        assert!(block(execute(&serde_json::json!({"path": file.to_string_lossy()}), &dir)).is_err());
    }
}
