//! `edit`: exact string replacement.
//!
//! The replacement is literal, not a regex, and it must match exactly once by default so
//! a stale line number cannot silently rewrite the wrong place. The diff returned with
//! the result is what the UI paints in red and green.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::ToolOutput;
use crate::ui::diff;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "edit".into(),
        description: "把文件中的一段文本替换为另一段。old_text 必须与文件内容逐字一致，\
                      默认要求唯一匹配；需要替换全部出现时把 replace_all 设为 true。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对当前工作目录或绝对路径）" },
                "old_text": { "type": "string", "description": "要被替换的原文，逐字一致" },
                "new_text": { "type": "string", "description": "替换后的新文本" },
                "replace_all": { "type": "boolean", "description": "是否替换全部匹配（默认 false）" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
    }
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let path = crate::tools::required_str(arguments, "path")?;
    let old_text = crate::tools::required_str(arguments, "old_text")?;
    let new_text = crate::tools::required_str(arguments, "new_text")?;
    let replace_all = arguments
        .get("replace_all")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if old_text.is_empty() {
        return Err("old_text 不能为空".into());
    }
    if old_text == new_text {
        return Err("old_text 与 new_text 相同，没有需要修改的内容".into());
    }
    let resolved = crate::auth::policy::resolve_tool_path(path, cwd);
    let before = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|err| format!("无法读取 {path}：{err}"))?;
    let occurrences = before.matches(old_text).count();
    if occurrences == 0 {
        return Err(format!(
            "在 {path} 中找不到 old_text。请先 read 该文件确认当前内容（注意缩进与换行必须逐字一致）。"
        ));
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "{path} 中 old_text 出现了 {occurrences} 次，无法确定要改哪一处；\
             请给出更长的上下文，或设置 replace_all=true。"
        ));
    }
    let after = if replace_all {
        before.replace(old_text, new_text)
    } else {
        before.replacen(old_text, new_text, 1)
    };
    tokio::fs::write(&resolved, &after)
        .await
        .map_err(|err| format!("无法写入 {path}：{err}"))?;
    let replaced = if replace_all { occurrences } else { 1 };
    let display = diff::for_edit(&before, &after);
    Ok(ToolOutput {
        content: format!("已修改 {path}（替换 {replaced} 处）"),
        display,
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;


    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpi-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn replaces_a_unique_occurrence() {
        let dir = temp_dir();
        let path = file(&dir, "a.txt", "one\ntwo\nthree\n");
        let out = block(execute(
            &serde_json::json!({"path": path, "old_text": "two", "new_text": "TWO"}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("替换 1 处"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
    }

    #[test]
    fn ambiguous_matches_are_refused_unless_replace_all() {
        let dir = temp_dir();
        let path = file(&dir, "b.txt", "x\nx\n");
        let error = block(execute(
            &serde_json::json!({"path": path, "old_text": "x", "new_text": "y"}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("出现了 2 次"));
        block(execute(
            &serde_json::json!({"path": path, "old_text": "x", "new_text": "y", "replace_all": true}),
            &dir,
        ))
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\n");
    }

    #[test]
    fn a_missing_match_says_what_to_do() {
        let dir = temp_dir();
        let path = file(&dir, "c.txt", "hello\n");
        let error = block(execute(
            &serde_json::json!({"path": path, "old_text": "nope", "new_text": "x"}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("找不到 old_text"));
        assert!(error.contains("请先 read"));
    }

    #[test]
    fn identical_text_is_rejected_early() {
        let dir = temp_dir();
        let path = file(&dir, "d.txt", "same\n");
        let error = block(execute(
            &serde_json::json!({"path": path, "old_text": "same", "new_text": "same"}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("没有需要修改的内容"));
    }
}
