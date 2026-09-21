//! `grep`: content search. Uses system `rg` when present, otherwise `grep -rn`.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};
use crate::util;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "grep".into(),
        description: "在文件内容中搜索。优先使用系统 rg，回退 grep。输出为「文件:行号:内容」。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "要搜索的文本或正则" },
                "path": { "type": "string", "description": "搜索目录或文件，默认当前目录" },
                "glob": { "type": "string", "description": "文件名过滤，如 '*.rs'" },
                "ignore_case": { "type": "boolean", "description": "是否忽略大小写" },
                "fixed_strings": { "type": "boolean", "description": "把 pattern 当作纯文本而不是正则" },
                "context": { "type": "integer", "description": "每条匹配前后附带的上下文行数" },
                "max_results": { "type": "integer", "description": "最多返回多少条匹配" }
            },
            "required": ["pattern"]
        }),
    }
}

/// Where the search tool lives, and which dialect it speaks.
enum Engine {
    Ripgrep(std::path::PathBuf),
    GnuGrep(std::path::PathBuf),
}

fn engine() -> Option<Engine> {
    for candidate in ["/usr/bin/rg", "/usr/local/bin/rg", "/bin/rg"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(Engine::Ripgrep(path.to_path_buf()));
        }
    }
    for candidate in ["/usr/bin/grep", "/bin/grep"] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(Engine::GnuGrep(path.to_path_buf()));
        }
    }
    None
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let pattern = crate::tools::required_str(arguments, "pattern")?;
    if pattern.is_empty() {
        return Err("pattern 不能为空".into());
    }
    let target = arguments.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let target_path = crate::auth::policy::resolve_tool_path(target, cwd);
    if !target_path.exists() {
        return Err(format!("路径不存在：{target}"));
    }
    let glob = arguments.get("glob").and_then(|v| v.as_str());
    let ignore_case = arguments.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false);
    let fixed = arguments.get("fixed_strings").and_then(|v| v.as_bool()).unwrap_or(false);
    let context = crate::tools::optional_u64(arguments, "context").unwrap_or(0);
    let max_results = crate::tools::optional_u64(arguments, "max_results").unwrap_or(200) as usize;

    let engine = engine().ok_or("系统中既没有 rg 也没有 grep，无法搜索")?;
    let mut command = tokio::process::Command::new(match &engine {
        Engine::Ripgrep(path) | Engine::GnuGrep(path) => path,
    });
    match &engine {
        Engine::Ripgrep(_) => {
            command.arg("--line-number").arg("--no-heading").arg("--color=never");
            if ignore_case {
                command.arg("--ignore-case");
            }
            if fixed {
                command.arg("--fixed-strings");
            }
            if let Some(glob) = glob {
                command.arg("--glob").arg(glob);
            }
            if context > 0 {
                command.arg("--context").arg(context.to_string());
            }
            command.arg("--max-count").arg(max_results.to_string());
            command.arg("--").arg(pattern).arg(&target_path);
        }
        Engine::GnuGrep(_) => {
            command.arg("-rn").arg("--color=never");
            if ignore_case {
                command.arg("-i");
            }
            if fixed {
                command.arg("-F");
            }
            if let Some(glob) = glob {
                command.arg(format!("--include={glob}"));
            }
            if context > 0 {
                command.arg(format!("-C{context}"));
            }
            command.arg("-m").arg(max_results.to_string());
            command.arg("--").arg(pattern).arg(&target_path);
        }
    }
    let output = command
        .current_dir(cwd)
        .output()
        .await
        .map_err(|err| format!("搜索命令启动失败：{err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let body = util::sanitize(&stdout);
    let matched = body.lines().count();
    if body.trim().is_empty() {
        let mut content = "没有找到匹配。".to_string();
        if !stderr.trim().is_empty() {
            content.push('\n');
            content.push_str(&util::sanitize(&stderr));
        }
        return Ok(ToolOutput {
            content,
            display: Display::File { verb: "搜索", path: target.to_string() },
            is_error: false,
duration: None,
});
    }
    let mut content = body.clone();
    content.push_str(&format!("\n[共 {matched} 行匹配]\n"));
    if !stderr.trim().is_empty() {
        content.push_str(&util::sanitize(&stderr));
    }
    Ok(ToolOutput {
        content,
        display: Display::File { verb: "搜索", path: target.to_string() },
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
        let dir = std::env::temp_dir().join(format!("pi-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.join("b.txt"), "gamma\nbeta\n").unwrap();
        dir
    }

    #[test]
    fn finds_matches_with_file_and_line_numbers() {
        let dir = fixture();
        let out = block(execute(&serde_json::json!({"pattern": "beta"}), &dir)).unwrap();
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("b.txt"));
        assert!(out.content.contains("2:beta") || out.content.contains("2:beta"));
        assert!(out.content.contains("共 2 行匹配"));
    }

    #[test]
    fn reports_no_matches_without_failing() {
        let dir = fixture();
        let out = block(execute(&serde_json::json!({"pattern": "nosuchthing"}), &dir)).unwrap();
        assert!(out.content.contains("没有找到匹配"));
        assert!(!out.is_error);
    }

    #[test]
    fn case_insensitivity_and_globs_are_passed_through() {
        let dir = fixture();
        let out = block(execute(
            &serde_json::json!({"pattern": "ALPHA", "ignore_case": true, "glob": "*.txt"}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("alpha"));
        let limited = block(execute(
            &serde_json::json!({"pattern": "beta", "glob": "a.txt"}),
            &dir,
        ))
        .unwrap();
        assert!(limited.content.contains("a.txt"));
        assert!(!limited.content.contains("b.txt"));
    }

    #[test]
    fn an_engine_is_always_available_on_this_platform() {
        assert!(engine().is_some());
    }

    #[test]
    fn an_empty_pattern_is_rejected() {
        let dir = fixture();
        assert!(block(execute(&serde_json::json!({"pattern": ""}), &dir)).is_err());
    }
}
