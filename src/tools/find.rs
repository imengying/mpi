//! `find`: locate files by name. Uses system `fd` when present, otherwise `find`.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};
use crate::util;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "find".into(),
        description: "按文件名查找文件。优先使用系统 fd，回退 find。返回相对路径列表。".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "文件名匹配，如 '*.rs' 或 'main'" },
                "path": { "type": "string", "description": "搜索目录，默认当前目录" },
                "type": { "type": "string", "enum": ["file", "dir"], "description": "只要文件或只要目录" },
                "max_depth": { "type": "integer", "description": "最大搜索深度" },
                "max_results": { "type": "integer", "description": "最多返回多少个结果" }
            },
            "required": ["pattern"]
        }),
    }
}

enum Engine {
    Fd(std::path::PathBuf),
    GnuFind(std::path::PathBuf),
}

fn engine() -> Option<Engine> {
    if let Some(path) = super::first_present(&["/usr/bin/fd", "/usr/local/bin/fd", "/bin/fd"]) {
        return Some(Engine::Fd(path));
    }
    super::first_present(&["/usr/bin/find", "/bin/find"]).map(Engine::GnuFind)
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let pattern = crate::tools::required_str(arguments, "pattern")?;
    if pattern.is_empty() {
        return Err("pattern 不能为空".into());
    }
    let target = arguments.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let target_path = crate::auth::policy::resolve_tool_path(target, cwd);
    if !target_path.is_dir() {
        return Err(format!("路径不是目录：{target}"));
    }
    let kind = arguments.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let max_depth = crate::tools::optional_u64(arguments, "max_depth");
    let max_results = crate::tools::optional_u64(arguments, "max_results").unwrap_or(500);

    let engine = engine().ok_or("系统中既没有 fd 也没有 find，无法查找")?;
    let mut command = tokio::process::Command::new(match &engine {
        Engine::Fd(path) | Engine::GnuFind(path) => path,
    });
    match &engine {
        Engine::Fd(_) => {
            command.arg("--color=never").arg("--hidden");
            match kind {
                "file" => {
                    command.arg("--type").arg("f");
                }
                "dir" => {
                    command.arg("--type").arg("d");
                }
                _ => {}
            }
            if let Some(depth) = max_depth {
                command.arg("--max-depth").arg(depth.to_string());
            }
            command.arg("--max-results").arg(max_results.to_string());
            // fd matches on a substring by default; globs have to opt in.
            if pattern.contains('*') || pattern.contains('?') {
                command.arg("--glob");
            }
            command.arg("--").arg(pattern).arg(&target_path);
        }
        Engine::GnuFind(_) => {
            // GNU find wants the paths before any expression, so `-maxdepth` follows.
            command.arg(&target_path);
            if let Some(depth) = max_depth {
                command.arg("-maxdepth").arg(depth.to_string());
            }
            match kind {
                "file" => {
                    command.arg("-type").arg("f");
                }
                "dir" => {
                    command.arg("-type").arg("d");
                }
                _ => {}
            }
            command.arg("-name").arg(pattern);
        }
    }
    let output = command
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| format!("查找命令启动失败：{err}"))?;
    let stdout = util::sanitize(String::from_utf8_lossy(&output.stdout).as_ref());
    let stderr = util::sanitize(String::from_utf8_lossy(&output.stderr).as_ref());
    let mut lines: Vec<String> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            // Present paths relative to the search root so the transcript stays short.
            let path = Path::new(line);
            match path.strip_prefix(&target_path) {
                Ok(rest) if !rest.as_os_str().is_empty() => rest.display().to_string(),
                _ => line.to_string(),
            }
        })
        .collect();
    let truncated = lines.len() as u64 > max_results && matches!(engine, Engine::GnuFind(_));
    if truncated {
        lines.truncate(max_results as usize);
    }
    if lines.is_empty() {
        let mut content = "没有找到匹配的文件。".to_string();
        if !stderr.trim().is_empty() {
            content.push('\n');
            content.push_str(&stderr);
        }
        return Ok(ToolOutput {
            content,
            display: Display::File { verb: "查找", path: target.to_string() },
            is_error: false,
duration: None,
});
    }
    let mut content = lines.join("\n");
    content.push('\n');
    if truncated {
        // The one thing the rows cannot say for themselves: this is a capped list, not the
        // whole answer. A note repeating the count was removed — anyone reading can count
        // the lines — but silently returning a prefix would read as "there is nothing else".
        content.push_str(&format!("[已截断，仅列出前 {} 个]\n", lines.len()));
    }
    Ok(ToolOutput {
        content,
        display: Display::File { verb: "查找", path: target.to_string() },
        is_error: false,
        duration: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;


    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-find-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("top.rs"), "").unwrap();
        std::fs::write(dir.join("sub/deep.rs"), "").unwrap();
        std::fs::write(dir.join("sub/note.txt"), "").unwrap();
        dir
    }

    #[test]
    fn finds_files_by_name_and_stays_relative() {
        let dir = fixture();
        let out = block(execute(&serde_json::json!({"pattern": "*.rs"}), &dir)).unwrap();
        assert!(out.content.contains("top.rs"));
        assert!(out.content.contains("sub/deep.rs"));
        assert!(!out.content.contains("note.txt"));
        // No count trailer on a complete result: the rows are the answer.
        assert!(!out.content.contains("结果"), "{:?}", out.content);
    }

    #[test]
    fn a_capped_result_says_it_was_capped() {
        // The count is not worth a row, but *this is a prefix* is: without it a capped list
        // reads as the whole answer, and the model would stop looking for what it did not see.
        let dir = fixture();
        let out = block(execute(
            &serde_json::json!({"pattern": "*", "max_results": 2}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("已截断"), "{:?}", out.content);
    }

    #[test]
    fn type_and_depth_filters_apply() {
        let dir = fixture();
        let files = block(execute(&serde_json::json!({"pattern": "*", "type": "file"}), &dir)).unwrap();
        assert!(!files.content.contains("sub\n"));
        let shallow = block(
            execute(&serde_json::json!({"pattern": "*.rs", "max_depth": 1}), &dir),
        )
        .unwrap();
        assert!(shallow.content.contains("top.rs"));
        assert!(!shallow.content.contains("deep.rs"));
    }

    #[test]
    fn a_non_directory_target_is_rejected() {
        let dir = fixture();
        let file = dir.join("top.rs");
        assert!(block(execute(&serde_json::json!({"pattern": "x", "path": file.to_string_lossy()}), &dir)).is_err());
    }
}
