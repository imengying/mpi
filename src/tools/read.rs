//! `read`: file contents with `offset` / `limit`, using the same display-width and line
//! numbering conventions as `cat -n` so the model can quote line numbers back.

use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::ToolOutput;

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
    let offset = crate::tools::optional_u64(arguments, "offset").unwrap_or(1).max(1);
    let limit = crate::tools::optional_u64(arguments, "limit").unwrap_or(2000);
    let out = tokio::task::spawn_blocking(move || read_window(&resolved, offset, limit))
        .await.map_err(|err| format!("读取任务失败：{err}"))??;

    Ok(ToolOutput {
        content: out,
        display: crate::tools::Display::File { verb: "读取", path: path.to_string() },
        is_error: false,
        duration: None,
    })
}

/// Scan in fixed-size buffers, including when a single line is gigabytes long.
fn read_window(path: &Path, offset: u64, limit: u64) -> Result<String, String> {
    use std::io::{BufRead, Read, Seek, SeekFrom, Write};
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut probe = [0; 4096];
        let count = file.read(&mut probe)?;
        if probe[..count].contains(&0) {
            return Err(std::io::Error::other("看起来是二进制文件，未读取"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut reader = std::io::BufReader::new(file);
        let mut output = super::output::Capture::new()?;
        let end = offset.saturating_add(limit);
        let mut number = 1u64;
        let mut prefixed = false;
        loop {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                if number < offset {
                    return Err(std::io::Error::other(format!(
                        "offset {offset} 超出文件行数（共 {number} 行）")));
                }
                if number >= offset && number < end {
                    if !prefixed { write!(output.file, "{number}\t")?; }
                    writeln!(output.file)?;
                }
                break;
            }
            if number >= end { break; }
            let newline = buf.iter().position(|b| *b == b'\n');
            let used = newline.map_or(buf.len(), |at| at + 1);
            if number >= offset {
                if !prefixed {
                    write!(output.file, "{number}\t")?;
                    prefixed = true;
                }
                output.file.write_all(&buf[..used])?;
            }
            reader.consume(used);
            if newline.is_some() {
                number += 1;
                prefixed = false;
            }
        }
        output.finish(false).map(|(text, _)| text)
    };
    read().map_err(|err| format!("无法读取 {}：{err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;

    #[test]
    fn skips_a_huge_line_and_handles_a_limit_that_would_overflow() {
        use std::io::Write;
        let dir = temp_dir();
        let path = dir.join("huge-line.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        let chunk = [b'a'; 8192];
        for _ in 0..256 { file.write_all(&chunk).unwrap(); }
        file.write_all("\n目标\n结尾".as_bytes()).unwrap();
        assert_eq!(read_window(&path, 2, 1).unwrap(), "2\t目标");
        assert_eq!(read_window(&path, 2, u64::MAX).unwrap(), "2\t目标\n3\t结尾");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_huge_selected_line_is_bounded_and_saved_in_full() {
        use std::io::Write;
        let dir = temp_dir();
        let path = dir.join("huge-selected.txt");
        let mut file = std::fs::File::create(&path).unwrap();
        for _ in 0..32 { file.write_all(&[b'x'; 8192]).unwrap(); }
        let text = read_window(&path, 1, 1).unwrap();
        assert!(text.len() <= crate::util::MAX_OUTPUT_BYTES);
        let log = text.split("完整输出：").nth(1).unwrap().trim_end_matches(']');
        assert!(std::fs::metadata(log).unwrap().len() > 262144);
        std::fs::remove_file(log).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }


    #[test]
    fn reads_with_line_numbers_and_no_trailing_note() {
        // The window is described by the tool's own arguments (offset / limit) and by the
        // line numbers on every row, so a note announcing it was a line the user had to read
        // on every windowed read to be told something the output already showed. It was also
        // the row that survived collapsing — the block keeps its last rows — so a long read
        // showed nothing *but* the note.
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
        assert!(!out.content.contains("仅显示"), "no window note: {:?}", out.content);
        assert!(!out.content.contains("offset="), "no follow-up advice: {:?}", out.content);
        // The rows are the output: nothing before them, nothing after them.
        assert_eq!(out.content.lines().count(), 2, "{:?}", out.content);
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
