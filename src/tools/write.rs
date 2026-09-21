//! `write`: create or replace a file, creating parent directories as needed.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::ToolOutput;
use crate::ui::diff;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "write".into(),
        description: "写入文件，自动创建父目录。会覆盖已有内容；修改已有文件时优先用 edit。".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对当前工作目录或绝对路径）" },
                "content": { "type": "string", "description": "完整文件内容" }
            },
            "required": ["path", "content"]
        }),
    }
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let path = crate::tools::required_str(arguments, "path")?;
    let content = crate::tools::required_str(arguments, "content")?;
    let resolved = crate::auth::policy::resolve_tool_path(path, cwd);
    let before = std::fs::read_to_string(&resolved).ok();
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| format!("无法创建目录 {}：{err}", parent.display()))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|err| format!("无法写入 {path}：{err}"))?;
    let existed = before.is_some();
    let display = match &before {
        Some(before) => diff::for_write(before, content),
        None => diff::for_new_file(content),
    };
    let action = if existed { "已覆盖" } else { "已创建" };
    Ok(ToolOutput {
        content: format!("{action} {path}（{} 字节）", content.len()),
        display,
        is_error: false,
duration: None,
})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;


    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir();
        let target = dir.join("nested/deeper/out.txt");
        let _ = std::fs::remove_dir_all(dir.join("nested"));
        let out = block(execute(
            &serde_json::json!({"path": target.to_string_lossy(), "content": "hello\n"}),
            &dir,
        ))
        .unwrap();
        assert!(target.exists());
        assert!(out.content.contains("已创建"));
    }

    #[test]
    fn overwriting_reports_a_diff() {
        let dir = temp_dir();
        let target = dir.join("rewrite.txt");
        std::fs::write(&target, "old\n").unwrap();
        let out = block(execute(
            &serde_json::json!({"path": target.to_string_lossy(), "content": "new\n"}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("已覆盖"));
        match out.display {
            crate::tools::Display::Diff { added, removed, .. } => {
                assert_eq!((added, removed), (1, 1));
            }
            other => panic!("expected a diff, got {other:?}"),
        }
    }
}
