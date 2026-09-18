//! `read`: file contents with `offset` / `limit`, using the same display-width and line
//! numbering conventions as `cat -n` so the model can quote line numbers back.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::ToolOutput;
use crate::util;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "read".into(),
        description: "读取文件内容。可选 offset（起始行号，从 1 开始）与 limit（最多读取行数）。\
                      输出带行号，便于后续 edit 引用。"
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径（相对当前工作目录或绝对路径）" },
                "offset": { "type": "integer", "description": "起始行号，从 1 开始" },
                "limit": { "type": "integer", "description": "最多读取的行数" }
            },
            "required": ["path"]
        }),
    }
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    let path = crate::tools::required_str(arguments, "path")?;
    let resolved = crate::auth::policy::resolve_tool_path(path, cwd);
    let meta = std::fs::metadata(&resolved).map_err(|err| format!("无法读取 {path}：{err}"))?;
    if meta.is_dir() {
        return Err(format!("{path} 是目录；请用 ls 或 find"));
    }
    let raw = std::fs::read(&resolved).map_err(|err| format!("无法读取 {path}：{err}"))?;
    if raw.iter().take(4096).any(|byte| *byte == 0) {
        return Err(format!("{path} 看起来是二进制文件，未读取"));
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let offset = crate::tools::optional_u64(arguments, "offset").unwrap_or(1).max(1) as usize;
    let limit = crate::tools::optional_u64(arguments, "limit").unwrap_or(2000) as usize;
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    if offset > total {
        return Err(format!("offset {offset} 超出文件行数（共 {total} 行）"));
    }
    let end = (offset - 1 + limit).min(total);
    let mut out = String::new();
    let width = end.to_string().len();
    for (index, line) in lines[(offset - 1)..end].iter().enumerate() {
        let number = offset + index;
        out.push_str(&format!("{:>width$}\t{line}\n", number, width = width));
    }
    if end < total {
        out.push_str(&format!(
            "\n[仅显示第 {offset}–{end} 行，共 {total} 行；继续读取请用 offset={}]\n",
            end + 1
        ));
    }
    let _ = util::estimate_tokens(&out);
    Ok(ToolOutput {
        content: out,
        display: crate::tools::Display::File { verb: "读取", path: path.to_string() },
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mpi-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }


    #[test]
    fn reads_with_line_numbers_and_reports_the_window() {
        let dir = temp_dir();
        let file = dir.join("sample.txt");
        std::fs::write(&file, "a\nb\nc\nd\ne\n").unwrap();
        let out = block(execute(
            &serde_json::json!({"path": file.to_string_lossy(), "offset": 2, "limit": 2}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("2\tb"));
        assert!(out.content.contains("3\tc"));
        assert!(!out.content.contains("1\ta"));
        assert!(out.content.contains("offset=4"));
    }

    #[test]
    fn a_directory_is_rejected_with_a_hint() {
        let dir = temp_dir();
        let error = block(execute(&serde_json::json!({"path": dir.to_string_lossy()}), &dir)).unwrap_err();
        assert!(error.contains("是目录"));
    }

    #[test]
    fn binary_files_are_not_dumped_into_the_transcript() {
        let dir = temp_dir();
        let file = dir.join("bin.dat");
        std::fs::write(&file, [0u8, 1, 2, 3, 4]).unwrap();
        let error = block(execute(&serde_json::json!({"path": file.to_string_lossy()}), &dir)).unwrap_err();
        assert!(error.contains("二进制"));
    }

    #[test]
    fn an_offset_past_the_end_is_an_error() {
        let dir = temp_dir();
        let file = dir.join("short.txt");
        std::fs::write(&file, "only\n").unwrap();
        let error = block(execute(
            &serde_json::json!({"path": file.to_string_lossy(), "offset": 9}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("超出文件行数"));
    }
}
