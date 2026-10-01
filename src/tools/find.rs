//! `find`: locate files by name. Uses system `fd` when present, otherwise `find`.

use std::io::{Seek, SeekFrom, Write};
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
    execute_with_engine(
        arguments,
        cwd,
        engine().ok_or("系统中既没有 fd 也没有 find，无法查找")?,
    )
    .await
}

async fn execute_with_engine(
    arguments: &serde_json::Value,
    cwd: &Path,
    engine: Engine,
) -> Result<ToolOutput, String> {
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
    if max_results == 0 {
        return Err("max_results 必须大于 0".into());
    }

    let mut command = tokio::process::Command::new(match &engine {
        Engine::Fd(path) | Engine::GnuFind(path) => path,
    });
    match &engine {
        Engine::Fd(_) => {
            command.arg("--color=never").arg("--hidden").arg("--print0");
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
            command
                .arg("--max-results")
                .arg(max_results.saturating_add(1).to_string());
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
            command.arg("-name").arg(pattern).arg("-print0");
        }
    }
    let output = super::process::capture(command.current_dir(cwd))
        .await
        .map_err(|err| format!("查找命令启动失败：{err}"))?;
    let body =
        tokio::task::spawn_blocking(move || select_paths(output.stdout, &target_path, max_results))
            .await
            .map_err(|err| err.to_string())??;
    let (stderr, _) = output.stderr.finish(true).map_err(|err| err.to_string())?;
    let failed = !output.status.success();
    let mut content = if body.is_empty() {
        if failed {
            format!("查找失败（退出码 {:?}）", output.status.code())
        } else {
            "没有找到匹配的文件。".into()
        }
    } else {
        body
    };
    if !stderr.trim().is_empty() {
        content.push('\n');
        content.push_str(&stderr);
    }
    Ok(ToolOutput {
        content,
        display: Display::File { verb: "查找", path: target.to_string() },
        is_error: failed,
        duration: None,
    })
}

fn select_paths(
    mut raw: super::output::Capture,
    target_path: &Path,
    max_results: u64,
) -> Result<String, String> {
    raw.file
        .seek(SeekFrom::Start(0))
        .map_err(|err| err.to_string())?;
    let mut reader = std::io::BufReader::new(&mut raw.file);
    let mut selected = super::output::Capture::new().map_err(|err| err.to_string())?;
    let mut count = 0u64;
    while let Some(bytes) = super::output::field(&mut reader, 0, crate::util::MAX_OUTPUT_BYTES)
        .map_err(|err| err.to_string())?
    {
        if count == max_results {
            writeln!(selected.file, "[已截断，仅列出前 {max_results} 个]")
                .map_err(|err| err.to_string())?;
            break;
        }
        let line = String::from_utf8_lossy(&bytes);
        let path = Path::new(line.as_ref());
        let relative = path
            .strip_prefix(target_path)
            .ok()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(path);
        writeln!(
            selected.file,
            "{}",
            util::one_line(&relative.to_string_lossy())
        )
        .map_err(|err| err.to_string())?;
        count += 1;
    }
    selected
        .finish(true)
        .map(|(body, _)| body)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;

    #[test]
    fn gnu_fallback_caps_results_without_splitting_filenames_at_newlines() {
        let dir = std::env::temp_dir().join(format!("pi-find-gnu-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        for name in ["line\nbreak.txt", "second.txt", "third.txt"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let binary = super::super::first_present(&["/usr/bin/find", "/bin/find"]).unwrap();
        let full = block(execute_with_engine(
            &serde_json::json!({"pattern":"*.txt"}),
            &dir,
            Engine::GnuFind(binary.clone()),
        ))
        .unwrap();
        assert_eq!(full.content.lines().count(), 3, "{}", full.content);
        let out = block(execute_with_engine(
            &serde_json::json!({"pattern":"*.txt", "max_results":2}),
            &dir,
            Engine::GnuFind(binary),
        ))
        .unwrap();
        assert_eq!(
            out.content
                .lines()
                .filter(|line| !line.starts_with('['))
                .count(),
            2
        );
        assert!(out.content.contains("已截断"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_large_file_list_keeps_a_bounded_excerpt_and_full_log() {
        let dir = std::env::temp_dir().join(format!("pi-find-large-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        for index in 0..800 {
            std::fs::write(dir.join(format!("{index:04}-{}.txt", "x".repeat(100))), "").unwrap();
        }
        let out = block(execute(
            &serde_json::json!({"pattern":"*.txt", "max_results":1000}),
            &dir,
        ))
        .unwrap();
        assert!(!out.is_error && out.content.len() <= crate::util::MAX_OUTPUT_BYTES);
        let path = out
            .content
            .split("完整输出：")
            .nth(1)
            .unwrap()
            .trim_end_matches(']');
        let full = std::fs::read_to_string(path).unwrap();
        assert_eq!(full.lines().count(), 800);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }


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
