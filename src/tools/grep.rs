//! `grep`: content search. Uses system `rg` when present, otherwise `grep -rn`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::llm::ToolSpec;
use crate::tools::{Display, ToolOutput};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "grep".into(),
        description: "在文件内容中搜索。优先使用系统 rg，回退 grep。输出为「文件:行号:内容」。\
                      递归搜索默认跳过凭据、密钥及敏感目录；搜索敏感内容请明确指定其路径并获得授权。"
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
    if let Some(path) = super::first_present(&["/usr/bin/rg", "/usr/local/bin/rg", "/bin/rg"]) {
        return Some(Engine::Ripgrep(path));
    }
    super::first_present(&["/usr/bin/grep", "/bin/grep"]).map(Engine::GnuGrep)
}

pub async fn execute(arguments: &serde_json::Value, cwd: &Path) -> Result<ToolOutput, String> {
    execute_with_engine(
        arguments,
        cwd,
        engine().ok_or("系统中既没有 rg 也没有 grep，无法搜索")?,
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
    if !target_path.exists() {
        return Err(format!("路径不存在：{target}"));
    }
    let filter_secrets = target_path.is_dir()
        && crate::auth::policy::assess_path(crate::auth::policy::Operation::Read, target, cwd)
            .allows();
    let glob = arguments.get("glob").and_then(|v| v.as_str());
    let ignore_case = arguments.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false);
    let fixed = arguments.get("fixed_strings").and_then(|v| v.as_bool()).unwrap_or(false);
    let context = crate::tools::optional_u64(arguments, "context").unwrap_or(0);
    let max_results = crate::tools::optional_u64(arguments, "max_results").unwrap_or(200);
    if max_results == 0 {
        return Err("max_results 必须大于 0".into());
    }

    let mut command = tokio::process::Command::new(match &engine {
        Engine::Ripgrep(path) | Engine::GnuGrep(path) => path,
    });
    match &engine {
        Engine::Ripgrep(_) => {
            command.args([
                "--no-config",
                "--line-number",
                "--with-filename",
                "--null",
                "--no-heading",
                "--context-separator=",
                "--color=never",
            ]);
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
            command
                .arg("--max-count")
                .arg(max_results.saturating_add(1).to_string());
            if filter_secrets {
                command.args(crate::auth::policy::search_exclusions(true));
            }
            command.arg("--").arg(pattern).arg(&target_path);
        }
        Engine::GnuGrep(_) => {
            command.args(["-rInH", "-Z", "--no-group-separator", "--color=never"]);
            command.env_remove("POSIXLY_CORRECT");
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
            command
                .arg("-m")
                .arg(max_results.saturating_add(1).to_string());
            if filter_secrets {
                command.args(crate::auth::policy::search_exclusions(false));
            }
            command.arg("--").arg(pattern).arg(&target_path);
        }
    }
    let output = super::process::capture(command.current_dir(cwd))
        .await
        .map_err(|err| format!("搜索命令启动失败：{err}"))?;
    let failed = !matches!(output.status.code(), Some(0 | 1));
    let body = tokio::task::spawn_blocking(move || select_matches(output.stdout, max_results))
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| format!("读取搜索输出失败：{err}"))?;
    let (stderr, _) = output.stderr.finish(true).map_err(|err| err.to_string())?;
    let mut content = if body.is_empty() {
        if failed {
            format!("搜索失败（退出码 {:?}）", output.status.code())
        } else {
            "没有找到匹配。".into()
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
        display: Display::File { verb: "搜索", path: target.to_string() },
        is_error: failed,
        duration: None,
    })
}

fn select_matches(mut raw: super::output::Capture, max_results: u64) -> std::io::Result<String> {
    raw.file.seek(SeekFrom::Start(0))?;
    let mut reader = std::io::BufReader::new(&mut raw.file);
    let mut selected = super::output::Capture::new()?;
    let mut count = 0u64;
    while let Some(path) = super::output::field(&mut reader, 0, crate::util::MAX_OUTPUT_BYTES)? {
        // Ripgrep emits an empty separator line between distant context groups.
        let path = path.strip_prefix(b"\n").unwrap_or(&path);
        let mut number = Vec::new();
        let separator = loop {
            let mut byte = [0];
            reader.read_exact(&mut byte)?;
            if matches!(byte[0], b':' | b'-') {
                break byte[0];
            }
            if !byte[0].is_ascii_digit() || number.len() >= 20 {
                return Err(std::io::Error::other("无法识别搜索行号"));
            }
            number.push(byte[0]);
        };
        if separator == b':' {
            if count == max_results {
                writeln!(selected.file, "[已截断，仅返回前 {max_results} 条匹配]")?;
                break;
            }
            count += 1;
        }
        selected.file.write_all(path)?;
        selected.file.write_all(&[separator])?;
        selected.file.write_all(&number)?;
        selected.file.write_all(&[separator])?;
        super::output::copy_line(&mut reader, &mut selected.file)?;
    }
    selected.finish(true).map(|(text, _)| text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;

    #[test]
    fn gnu_fallback_enforces_the_same_limits_and_secret_filters() {
        let dir = fixture("gnu-fallback");
        std::fs::write(dir.join(".env"), "beta\n").unwrap();
        let input = serde_json::json!({"pattern":"beta", "context":1, "max_results":1});
        let engine =
            Engine::GnuGrep(super::super::first_present(&["/usr/bin/grep", "/bin/grep"]).unwrap());
        let out = block(execute_with_engine(&input, &dir, engine)).unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.content
                .lines()
                .filter(|line| line.ends_with(":beta"))
                .count(),
            1,
            "{}",
            out.content
        );
        assert!(out.content.contains("已截断"));
        assert!(!out.content.contains(".env"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_match_limit_is_global_and_preserves_context() {
        let dir = fixture("global-limit");
        let out = block(execute(
            &serde_json::json!({"pattern":"beta", "context":1, "max_results":1}),
            &dir,
        ))
        .unwrap();
        assert_eq!(
            out.content
                .lines()
                .filter(|line| line.ends_with(":beta"))
                .count(),
            1,
            "{}",
            out.content
        );
        assert!(out.content.contains("已截断"));
        assert!(
            out.content.contains("-1-")
                && (out.content.contains("alpha") || out.content.contains("gamma"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recursive_search_cannot_restore_secrets_with_a_glob() {
        let dir = fixture("secrets");
        std::fs::write(dir.join(".env"), "search-sentinel\n").unwrap();
        std::fs::create_dir(dir.join(".ssh")).unwrap();
        std::fs::write(dir.join(".ssh/config"), "search-sentinel\n").unwrap();
        let out = block(execute(
            &serde_json::json!({"pattern":"search-sentinel", "glob":".env"}),
            &dir,
        ))
        .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("没有找到匹配"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_huge_match_is_bounded_and_retained_in_full() {
        let dir = fixture("huge-match");
        let line = format!("match{}\n", "x".repeat(2 * 1024 * 1024));
        std::fs::write(dir.join("huge.txt"), &line).unwrap();
        let out = block(execute(&serde_json::json!({"pattern":"match"}), &dir)).unwrap();
        assert!(out.content.len() <= crate::util::MAX_OUTPUT_BYTES);
        let path = out
            .content
            .split("完整输出：")
            .nth(1)
            .unwrap()
            .trim_end_matches(']');
        assert!(std::fs::metadata(path).unwrap().len() > line.len() as u64);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_patterns_report_an_error() {
        let dir = fixture("invalid-regex");
        let out = block(execute(&serde_json::json!({"pattern":"["}), &dir)).unwrap();
        assert!(out.is_error);
        assert!(!out.content.contains("没有找到匹配"));
        std::fs::remove_dir_all(dir).unwrap();
    }


    /// A directory holding `a.txt` and `b.txt`, private to one test.
    ///
    /// Named per test rather than per process: the tests run in parallel, and a shared
    /// directory means one test's `fs::write` truncates a file while another test's `rg`
    /// is reading it — the search then reports no match and the failure looks like a bug
    /// in the tool rather than a collision in the fixture.
    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pi-grep-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        std::fs::write(dir.join("b.txt"), "gamma\nbeta\n").unwrap();
        dir
    }

    #[test]
    fn finds_matches_with_file_and_line_numbers() {
        let dir = fixture("matches");
        let out = block(execute(&serde_json::json!({"pattern": "beta"}), &dir)).unwrap();
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("b.txt"));
        assert!(out.content.contains("2:beta") || out.content.contains("2:beta"));
        // No `[共 N 行匹配]` trailer: the matches are the answer, and the block keeps its
        // last rows when collapsed, so the note outlived the matches it was counting.
        assert!(!out.content.contains("行匹配"), "{:?}", out.content);
    }

    #[test]
    fn reports_no_matches_without_failing() {
        let dir = fixture("no-matches");
        let out = block(execute(&serde_json::json!({"pattern": "nosuchthing"}), &dir)).unwrap();
        assert!(out.content.contains("没有找到匹配"));
        assert!(!out.is_error);
    }

    #[test]
    fn case_insensitivity_and_globs_are_passed_through() {
        let dir = fixture("globs");
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
        let dir = fixture("empty");
        assert!(block(execute(&serde_json::json!({"pattern": ""}), &dir)).is_err());
    }
}
