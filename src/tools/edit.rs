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
        .map_err(|err| format!("无法读取：{err}"))?;
    let occurrences = before.matches(old_text).count();
    if occurrences == 0 {
        return Err(format!(
            "找不到 old_text：它必须与文件内容逐字一致（含缩进与换行）。{}",
            nearest_region(&before, old_text)
        ));
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "old_text 出现了 {occurrences} 次，无法确定要改哪一处（可给更长的上下文，\
             或设 replace_all=true）。"
        ));
    }
    let after = if replace_all {
        before.replace(old_text, new_text)
    } else {
        before.replacen(old_text, new_text, 1)
    };
    tokio::fs::write(&resolved, &after)
        .await
        .map_err(|err| format!("无法写入：{err}"))?;
    let replaced = if replace_all { occurrences } else { 1 };
    let display = diff::for_edit(&before, &after);
    Ok(ToolOutput {
        content: format!("已修改（替换 {replaced} 处）"),
        display,
        is_error: false,
        duration: None,
    })
}

/// The line range whose text is most similar to `old_text`, as a hint after a failed match.
///
/// A miss is almost never a wrong file: the model edited from a stale or half-remembered
/// reading, so its `old_text` is real content that has since drifted — a duplicated line, a
/// changed indent, one renamed identifier. Telling it only "must match exactly" leaves it to
/// re-read the whole file and find that itself; naming the region it meant is the one fact the
/// caller cannot infer from its own arguments.
///
/// Bounded on purpose: at most [`HINT_LINES`] lines, and nothing at all when even the best
/// window is a poor match (in that case there is no "region it meant" to point at, and a
/// guess would be worse than silence).
fn nearest_region(before: &str, old_text: &str) -> String {
    const HINT_LINES: usize = 12;
    // Low enough to survive a real rewrite. The misses this hint explains measure 0.20-0.33
    // on their true region: an edited block where a rename or a changed value costs most of
    // the exact matches. 0.25 dropped the 0.20 case — the hint vanished precisely when the
    // drift was small. What it must still exclude is a caller whose text touches nothing in
    // the file, and that scores 0.
    const MIN_SCORE: f64 = 0.2;
    let lines: Vec<&str> = before.lines().collect();
    if lines.is_empty() || old_text.is_empty() {
        return String::new();
    }
    // The window height follows the caller's own text, so the hint covers about as much as
    // it asked about rather than a fixed guess at the right size.
    let height = old_text.lines().count().clamp(1, HINT_LINES);
    let wanted: Vec<&str> = old_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if wanted.is_empty() {
        return String::new();
    }
    let mut best = (0usize, 0f64);
    for start in 0..lines.len() {
        let window = &lines[start..(start + height).min(lines.len())];
        let matching = window
            .iter()
            .filter(|line| wanted.iter().any(|want| lines_are_alike(want, line.trim())))
            .count();
        let score = matching as f64 / wanted.len().max(1) as f64;
        if score > best.1 {
            best = (start, score);
        }
    }
    if best.1 < MIN_SCORE {
        return String::new();
    }
    let start = best.0;
    let end = (start + height).min(lines.len());
    let body: Vec<String> = (start..end)
        .map(|index| format!("{}\t{}", index + 1, lines[index]))
        .collect();
    format!(
        "\n最相似的位置在第 {}-{end} 行：\n{}",
        start + 1,
        body.join("\n")
    )
}

/// Whether two lines are close enough to be the same line after an edit.
///
/// Exact equality is the wrong test for the misses this hint exists to explain. A line that
/// was renamed a word (`.codexResetCreditTime` where the file now says `.codexResetCreditRow`)
/// or whose value changed is *the* line the caller meant, and counting it as a non-match can
/// push a score under the threshold — losing the hint exactly when the drift was small and the
/// hint would have been most useful.
///
/// The comparison is by shared prefix on the indent-stripped text, which is what a rename or
/// an edited value leaves behind. Short lines are excluded outright: `}` and `});` share their
/// first character, and at that length a prefix says nothing about which block is meant, so
/// they would let an unrelated window pass as a match.
fn lines_are_alike(want: &str, have: &str) -> bool {
    const MIN_PREFIX: usize = 8;
    if want == have {
        return true;
    }
    let want_len = want.chars().count();
    let have_len = have.chars().count();
    // A line too short to carry a distinctive prefix is never evidence of a match, even
    // against itself — the caller's `trim` already removed the indentation that would have
    // made a short line like `}` meaningful.
    if want_len < MIN_PREFIX || have_len < MIN_PREFIX {
        return false;
    }
    let shared = want
        .chars()
        .zip(have.chars())
        .take_while(|(a, b)| a == b)
        .count();
    shared >= MIN_PREFIX
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::block;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-edit-{}", std::process::id()));
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
        // The cause, not an instruction: the model reads this as a tool result, and it does
        // not need to be told to read the file it just failed to edit.
        assert!(error.contains("逐字一致"));
        assert!(!error.contains("请先 read"));
    }

    #[test]
    fn a_drifted_match_points_at_the_region_it_meant() {
        // The shape of every real miss in the sessions: the caller edited from a stale
        // reading, so its text is real content that has since changed by a line or an
        // indent. Naming the region is the one thing it cannot get from its own arguments.
        let dir = temp_dir();
        let path = file(
            &dir,
            "drift.txt",
            "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n",
        );
        let error = block(execute(
            &serde_json::json!({
                "path": path,
                "old_text": "    let a = 1;\n    let b = 2;\n    let bb = 2;",
                "new_text": "x",
            }),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("找不到 old_text"));
        assert!(error.contains("最相似的位置"), "{error}");
        // Numbered the way `read` numbers, so the caller can go straight there.
        assert!(error.contains("2\t    let a = 1;"), "{error}");
    }

    #[test]
    fn an_unrelated_miss_does_not_invent_a_region() {
        // Nothing in the file resembles the request, so there is no "region it meant".
        // A guess here would send the caller to a line that has nothing to do with it.
        let dir = temp_dir();
        let path = file(&dir, "other.txt", "alpha\nbeta\ngamma\n");
        let error = block(execute(
            &serde_json::json!({
                "path": path,
                "old_text": "完全不相干的一段文本，与文件内容没有任何重叠部分",
                "new_text": "x",
            }),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("找不到 old_text"));
        assert!(!error.contains("最相似的位置"), "{error}");
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

    #[test]
    fn a_renamed_line_still_counts_as_the_same_line() {
        // The shape of a real miss: the caller's block is right, but one line was renamed
        // under it (`.codexResetCreditTime` where the file now reads `.codexResetCreditRow`).
        // Counting that line as a non-match is what pushed the score under the threshold and
        // lost the hint in the cases where the drift was smallest.
        assert!(lines_are_alike(
            ".codexResetCreditTime {",
            ".codexResetCreditRow {"
        ));
        assert!(lines_are_alike("let value = 1;", "let value = 2;"));
        assert!(lines_are_alike("    return result;", "    return totals;"));

        // And the comparison is what `nearest_region` actually uses, not just what exists: here
        // *every* line drifted, so exact comparison scores 0 and finds nothing, while the shared
        // prefixes place the block. The closing brace is left out on purpose — an identical `}`
        // would match exactly and carry the score on its own, hiding whether prefixes count.
        let before = [
            "header",
            "    .codexResetCreditRow {",
            "      color: var(--text-primary);",
            "      min-width: 0;",
            "    }",
            "footer",
        ]
        .join("\n");
        let drifted = [
            "    .codexResetCreditTime {",
            "      color: var(--text-secondary);",
            "      min-width: 100%;",
        ]
        .join("\n");
        assert!(
            !nearest_region(&before, &drifted).is_empty(),
            "a block that drifted on every line must still be located"
        );

        // Near-misses that must not count, or every window would look plausible.
        assert!(!lines_are_alike("}", "});"));
        assert!(!lines_are_alike("alpha", "omega"));
        assert!(!lines_are_alike("let a = 1;", "const b = 2;"));
        // The comparison is a shared prefix, not a general diff: change the opening of a
        // short line and too little is left to match on. That limit is intended — guessing
        // a region from two shared characters would name the wrong one.
        assert!(!lines_are_alike("a: 1;", "b: 1;"));
    }

    #[test]
    fn a_heavily_rewritten_block_still_points_at_its_region() {
        // Taken from a recorded miss, where the caller's block was right but every line in it
        // had drifted: one renamed (`.codexResetCreditTime` where the file reads
        // `.codexResetCreditRow`), the rest holding different declarations. Only `min-width: 0;`
        // and the closing brace still matched exactly, which scores 0.20 — the old 0.25
        // threshold dropped it, so the hint vanished on the miss that needed it most.
        let before = [
            "  .codexResetCreditRow {",
            "    display: flex;",
            "    align-items: center;",
            "    justify-content: space-between;",
            "    gap: $spacing-sm;",
            "    padding: 6px 8px;",
            "    border: 1px solid var(--border-color);",
            "    font-size: 12px;",
            "    line-height: 1.35;",
            "    min-width: 0;",
            "  }",
        ]
        .join("\n");
        let old_text = [
            "    .codexResetCreditTime {",
            "      color: var(--text-primary);",
            "      font-weight: 600;",
            "      text-align: right;",
            "      overflow-wrap: anywhere;",
            "      min-width: 0;",
            "",
            "      .quotaResetRelative {",
            "        margin-left: 4px;",
            "      }",
            "    }",
        ]
        .join("\n");
        let hint = nearest_region(&before, &old_text);
        assert!(
            !hint.is_empty(),
            "a block at 0.20 on its true region must still be named"
        );
        // The window is where the block really is, and it is the only thing the caller
        // cannot derive from its own text.
        assert!(hint.contains(".codexResetCreditRow"), "{hint}");
    }

    #[test]
    fn text_touching_nothing_in_the_file_still_gets_no_region() {
        // The floor the threshold protects. Loosening the line comparison must not turn a
        // caller that is editing the wrong file into one that gets sent to a real line.
        let before = "alpha\nbeta\ngamma\n";
        for old_text in [
            "完全不相干的一段文本，与文件内容没有任何重叠部分",
            "delta\nepsilon\nzeta",
            "// nothing here resembles this\n// not even a little",
        ] {
            assert!(
                nearest_region(before, old_text).is_empty(),
                "{old_text:?} must not invent a region"
            );
        }
    }
}
