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
                "path": { "type": "string", "description": "文件路径（相对当前工作目录或绝对路径）。给出 edits 时，这里是各项的默认路径——改同一个文件的几处时只写这一处即可。先在参数里给出 path，再给 old_text 和 new_text" },
                "old_text": { "type": "string", "description": "要被替换的原文，逐字一致" },
                "new_text": { "type": "string", "description": "替换后的新文本" },
                "replace_all": { "type": "boolean", "description": "是否替换全部匹配（默认 false）" },
                "edits": {
                    "type": "array",
                    "description": "一次改多处（可跨文件）：每项给 old_text、new_text，可选 path 与 replace_all。\
                                    项里不给 path 就用顶层的 path，所以改同一个文件的几处只需在顶层写一次。\
                                    全部成功才写入，任何一处失败则整个调用不产生修改。",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "文件路径；不给则用顶层的 path。先在参数里给出 path，再给 old_text 和 new_text" },
                            "old_text": { "type": "string", "description": "要被替换的原文，逐字一致" },
                            "new_text": { "type": "string", "description": "替换后的新文本" },
                            "replace_all": { "type": "boolean", "description": "是否替换全部匹配（默认 false）" }
                        },
                        "required": ["path", "old_text", "new_text"]
                    }
                }
            },
            // Either shape is complete on its own: one replacement, or a batch that may span
            // files. Spelling it out keeps a caller from sending `path` without `old_text`
            // and getting a puzzling error.
            "anyOf": [
                { "required": ["path", "old_text", "new_text"] },
                { "required": ["edits"] }
            ]
        }),
    }
}

/// One replacement: which file, what to find, what to put there.
struct Edit<'a> {
    path: &'a str,
    old_text: &'a str,
    new_text: &'a str,
    replace_all: bool,
}

pub async fn execute(
    arguments: &serde_json::Value,
    cwd: &Path,
    observed: &crate::tools::observed::Observed,
) -> Result<ToolOutput, String> {
    let edits = match arguments.get("edits") {
        Some(serde_json::Value::Array(items)) => {
            let default_path = arguments.get("path").and_then(|value| value.as_str());
            parse_batch(items, default_path)?
        }
        Some(_) => return Err("edits 必须是数组".into()),
        None => vec![Edit {
            path: crate::tools::required_str(arguments, "path")?,
            old_text: crate::tools::required_str(arguments, "old_text")?,
            new_text: crate::tools::required_str(arguments, "new_text")?,
            replace_all: replace_all_of(arguments),
        }],
    };
    for edit in &edits {
        check_texts(edit.old_text, edit.new_text)?;
    }
    if edits.len() == 1 {
        return apply_one(&edits[0], cwd, observed).await;
    }
    apply_batch(&edits, cwd, observed).await
}

fn replace_all_of(value: &serde_json::Value) -> bool {
    value
        .get("replace_all")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// The reasons a replacement cannot mean anything, whatever file it points at.
fn check_texts(old_text: &str, new_text: &str) -> Result<(), String> {
    if old_text.is_empty() {
        return Err("old_text 不能为空".into());
    }
    if old_text == new_text {
        return Err("old_text 与 new_text 相同，没有需要修改的内容".into());
    }
    Ok(())
}

/// Read a batch, with the top-level `path` standing in for any item that omits one.
///
/// Three edits to one file is the ordinary shape — a rename applied throughout, a section
/// rewritten in a few places — and requiring `path` on every item made the natural way to
/// write it fail. A recorded session sent exactly that: `path` at the top, three items
/// without one, and got `第 1 项缺少 path`. It retried with the path repeated four times,
/// then went back to rewriting files through `python` heredocs (133 times in one session),
/// which is the path with no uniqueness check at all.
///
/// An item that names its own `path` still wins, so a batch across files works unchanged.
fn parse_batch<'a>(
    items: &'a [serde_json::Value],
    default_path: Option<&'a str>,
) -> Result<Vec<Edit<'a>>, String> {
    if items.is_empty() {
        return Err("edits 是空的，没有需要修改的内容".into());
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let field = |name: &str| {
                item.get(name)
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| format!("第 {} 项缺少 {name}", index + 1))
            };
            let path = item
                .get("path")
                .and_then(|value| value.as_str())
                .or(default_path)
                .ok_or_else(|| {
                    format!(
                        "第 {} 项没有 path，顶层也没有给出默认 path；\
                         多处修改同一个文件时把 path 写在顶层即可",
                        index + 1
                    )
                })?;
            Ok(Edit {
                path,
                old_text: field("old_text")?,
                new_text: field("new_text")?,
                replace_all: replace_all_of(item),
            })
        })
        .collect()
}

/// Replace one occurrence (or all of them) in one file and report the diff.
async fn apply_one(
    edit: &Edit<'_>,
    cwd: &Path,
    observed: &crate::tools::observed::Observed,
) -> Result<ToolOutput, String> {
    let resolved = crate::auth::policy::resolve_tool_path(edit.path, cwd);
    // Before matching, deliberately. A stale file makes `old_text` miss, and "not found" would
    // read as a typo — sending the model to guess at other spellings of text that is no longer
    // there. Naming the real cause is the whole point of tracking it.
    observed.stale(&resolved, edit.path)?;
    let before = tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|err| format!("无法读取：{err}"))?;
    let (after, replaced) = replace_in(&before, edit)?;
    tokio::fs::write(&resolved, &after)
        .await
        .map_err(|err| format!("无法写入：{err}"))?;
    // This batch made the change, so a later edit of the same file must not read as
    // someone else's.
    observed.forget(&resolved);
    Ok(ToolOutput {
        content: format!("已修改（替换 {replaced} 处）"),
        display: diff::for_edit(&before, &after),
        is_error: false,
        duration: None,
        images: Vec::new(),
    })
}

/// How strictly a located match agrees with what the caller wrote.
///
/// The caller's text drifts from the file in a few habitual ways, and refusing all of them
/// turns a nearly-right edit into a round trip: it re-reads the file and tries again, which is
/// the most common failure in the recorded sessions. `git apply` has always tolerated exactly
/// these, which is why a patch written against a lightly drifted file still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// Byte for byte.
    Exact,
    /// Same text once trailing whitespace is ignored on each line.
    TrailingWhitespace,
    /// Same text once leading and trailing whitespace are ignored on each line.
    Indentation,
    /// Same text once typographic punctuation is folded to ASCII.
    Punctuation,
}

impl Match {
    /// Whether the file's own text has to be used rather than the caller's.
    ///
    /// Anything looser than exact: the caller's text is a *description* of the region, and
    /// pasting it in would write its drifted indentation or its ASCII dashes into a file that
    /// has the real thing. The file's own bytes are what belong there.
    fn uses_file_text(self) -> bool {
        self != Match::Exact
    }
}

/// Apply the replacement to `before`, refusing an ambiguous or absent match.
fn replace_in(before: &str, edit: &Edit<'_>) -> Result<(String, usize), String> {
    let located = locate(before, edit.old_text, edit.replace_all)?;
    let mut after = before.to_string();
    // Applied back to front so that each replacement's offsets are still valid: working
    // forwards would shift every later one by the length of the text just inserted.
    for found in located.iter().rev() {
        let (from, to) = (found.start, found.end);
        // Substituting inside the caller's text keeps the lines it did not touch — a rename
        // buried in an indented block — while the file's own bytes supply everything else.
        let replacement = if found.strictness.uses_file_text() {
            retarget(&after[from..to], edit.old_text, edit.new_text)
        } else {
            edit.new_text.to_string()
        };
        after.replace_range(from..to, &replacement);
    }
    Ok((after, located.len()))
}

/// A stretch of `before` that stands in for the caller's `old_text`.
#[derive(Clone)]
struct Found {
    start: usize,
    end: usize,
    strictness: Match,
}

/// Find every stretch of `before` that `old_text` could mean, loosest match last.
///
/// Uniqueness is decided against the *loosest* reading that matches anything. If two places
/// both match exactly, the caller is ambiguous and has to say which. If two places match only
/// once indentation is ignored they are still two places, and picking the first would rewrite
/// code the caller never looked at.
fn locate(before: &str, old_text: &str, replace_all: bool) -> Result<Vec<Found>, String> {
    // An exact hit is a byte match, not a line match: substituting the caller's text for the
    // bytes it named must leave everything around it untouched, including the newline that
    // follows. Going through the line machinery here would replace a whole line with a string
    // that carries no terminator, joining it to the next one.
    let exact: Vec<Found> = before
        .match_indices(old_text)
        .map(|(start, found)| Found {
            start,
            end: start + found.len(),
            strictness: Match::Exact,
        })
        .collect();
    if !exact.is_empty() {
        return balance(exact, replace_all);
    }
    // Nothing matched byte for byte, so try the drift levels in order of strictness.
    for strictness in [
        Match::TrailingWhitespace,
        Match::Indentation,
        Match::Punctuation,
    ] {
        let found = find_all(before, old_text, strictness);
        if !found.is_empty() {
            return balance(found, replace_all);
        }
    }
    Err(format!(
        "找不到 old_text：它必须与文件内容逐字一致（含缩进与换行）。{}",
        nearest_region(before, old_text)
    ))
}

/// Apply the uniqueness rule to the matches found at one strictness level.
fn balance(found: Vec<Found>, replace_all: bool) -> Result<Vec<Found>, String> {
    if found.len() > 1 && !replace_all {
        return Err(format!(
            "old_text 出现了 {} 次，无法确定要改哪一处（可给更长的上下文，\
             或设 replace_all=true）。",
            found.len()
        ));
    }
    Ok(if replace_all {
        found
    } else {
        vec![found.into_iter().next().expect("non-empty")]
    })
}

/// The lines of `text` with their terminators, so a match can be mapped back to bytes.
fn lines_with_endings(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Every place the lines of `old_text` line up with the lines of `before` under `strictness`.
///
/// A match needs the same *number* of lines; what those lines contain is what `strictness`
/// decides. Line-by-line comparison rather than a substring search is what lets a drifted
/// indent be seen as the same line instead of a different one.
fn find_all(before: &str, old_text: &str, strictness: Match) -> Vec<Found> {
    let have = lines_with_endings(before);
    // The caller's text is a plain string: its last line usually has no terminator, and
    // comparing it against a file line that does would never match.
    let want: Vec<&str> = old_text.split_inclusive('\n').collect();
    if want.is_empty() || have.len() < want.len() {
        return Vec::new();
    }
    let mut offsets = Vec::with_capacity(have.len());
    let mut offset = 0usize;
    for line in &have {
        offsets.push(offset);
        offset += line.len();
    }
    let mut found = Vec::new();
    for start in 0..=have.len() - want.len() {
        let window = &have[start..start + want.len()];
        if window
            .iter()
            .zip(&want)
            .all(|(have, want)| lines_agree(have, want, strictness))
        {
            let begin = offsets[start];
            let end = begin + window.iter().map(|line| line.len()).sum::<usize>();
            found.push(Found {
                start: begin,
                end,
                strictness,
            });
        }
    }
    found
}

/// Whether one file line stands in for one line of the caller's text.
fn lines_agree(have: &str, want: &str, strictness: Match) -> bool {
    // The terminator is not part of the comparison: whether the last line of an edit carries
    // its newline is an artefact of how the caller wrote the string, not a statement about
    // the file. The length check above already matched line counts.
    let have = have.trim_end_matches('\n');
    let want = want.trim_end_matches('\n');
    match strictness {
        Match::Exact => have == want,
        Match::TrailingWhitespace => have.trim_end() == want.trim_end(),
        Match::Indentation => have.trim() == want.trim(),
        Match::Punctuation => fold_punctuation(have.trim()) == fold_punctuation(want.trim()),
    }
}

/// Typographic punctuation folded to its ASCII equivalent, for comparing a caller's text
/// against a file that uses the real characters.
///
/// Written text arrives through all sorts of editors: an em dash typed in a chat box, a curly
/// quote from a word processor, a full-width space from a Chinese input method. None of them
/// is a different *line* — folding them is how `git apply` treats the same drift.
fn fold_punctuation(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// The caller's `new_text`, re-expressed in the matched region's own bytes.
///
/// `found` and `old_text` describe the same lines at one of the looser levels, so working line
/// by line keeps every part of the caller's intent that did not drift — the words that changed
/// — while taking indentation and line endings from the file.
///
/// **Line endings come from the file, always.** Replacing a region substitutes bytes, and the
/// caller's `new_text` carries no terminator on its last line (a string literal rarely does).
/// Using it verbatim would join the replacement to the following line, which is how a fuzzy
/// match can corrupt a file that a strict match would have refused.
///
/// Line counts need not agree: a caller may collapse or expand a block. When they do not, the
/// replacement stands as written and only the ending of its last line is taken from the file.
fn retarget(found: &str, old_text: &str, new_text: &str) -> String {
    let file_lines: Vec<&str> = lines_with_endings(found);
    let want_lines: Vec<&str> = lines_with_endings(old_text);
    let new_lines: Vec<&str> = lines_with_endings(new_text);
    // The file's terminating newline, if the region has one. A region that ends at end-of-file
    // without a newline must stay that way.
    let trailing = file_lines
        .last()
        .map(|line| line.ends_with('\n'))
        .unwrap_or(false);
    let mut out = String::new();
    for (index, new) in new_lines.iter().enumerate() {
        let new_body = new.trim_end_matches('\n');
        let last = index + 1 == new_lines.len();
        let placed = match (file_lines.get(index), want_lines.get(index)) {
            (Some(file), Some(want)) => {
                let want_body = want.trim_end_matches('\n');
                if lines_agree(file, want, Match::Indentation) && want_body == new_body {
                    // Unchanged line: the file's own bytes, so nothing drifts.
                    file.trim_end_matches('\n').to_string()
                } else {
                    // Changed line: the caller's words at the file's indentation.
                    reindent(file, want_body, new_body)
                }
            }
            // The region grew: no file line to follow, so the caller's text stands.
            _ => new_body.to_string(),
        };
        out.push_str(&placed);
        // Every line but the last keeps a newline, because `new_lines` only had one to keep
        // where the caller wrote it. The last line follows the region it replaced.
        if !last || trailing || new.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Place `new_body` at the indentation the file's own line uses.
///
/// The caller indented its replacement to match the text it *thought* it was editing. When
/// that text had the wrong indent, its replacement carries the same error; re-basing it on the
/// file's line is what keeps a correct edit from landing in the wrong column.
fn reindent(file_line: &str, want_body: &str, new_body: &str) -> String {
    let file_indent: String = file_line
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let want_indent: String = want_body
        .chars()
        .take_while(|c| c.is_whitespace())
        .collect();
    let new_indent: String = new_body.chars().take_while(|c| c.is_whitespace()).collect();
    if want_indent == new_indent {
        // The caller did not change the indentation, so the file's own is the right one.
        format!("{file_indent}{}", &new_body[new_indent.len()..])
    } else {
        new_body.to_string()
    }
}

/// Apply several replacements as one transaction.
///
/// Nothing is written unless every replacement matches, because the caller asked for a set of
/// changes, not for as many of them as happen to apply: half-applied edits across several
/// files are a state neither the caller nor the user can reason about, and the caller has no
/// way to tell which half landed. Edits are applied in order, so two edits in the same file
/// see each other.
///
/// This is the shape a scripted rewrite used to take — the recorded sessions had 206 of them,
/// a `python` heredoc replacing a table of `(path, old, new)` entries — and it carried none of
/// the checks here: no uniqueness requirement, no all-or-nothing, and a mistake rewrote the
/// file silently.
async fn apply_batch(
    edits: &[Edit<'_>],
    cwd: &Path,
    observed: &crate::tools::observed::Observed,
) -> Result<ToolOutput, String> {
    // Group by file so each is read once and written once, keeping the caller's order.
    let mut order: Vec<std::path::PathBuf> = Vec::new();
    let mut pending: std::collections::HashMap<std::path::PathBuf, (String, String)> =
        std::collections::HashMap::new();
    let mut replaced_total = 0usize;
    for (index, edit) in edits.iter().enumerate() {
        let resolved = crate::auth::policy::resolve_tool_path(edit.path, cwd);
        if !pending.contains_key(&resolved) {
            observed.stale(&resolved, edit.path)?;
            let text = tokio::fs::read_to_string(&resolved)
                .await
                .map_err(|err| format!("第 {} 项无法读取 {}：{err}", index + 1, edit.path))?;
            pending.insert(resolved.clone(), (text.clone(), text));
            order.push(resolved.clone());
        }
        let (current, _) = pending.get(&resolved).expect("just inserted");
        let (next, replaced) = replace_in(current, edit)
            .map_err(|err| format!("第 {} 项（{}）：{err}", index + 1, edit.path))?;
        replaced_total += replaced;
        pending.get_mut(&resolved).expect("just inserted").0 = next;
    }
    // Every replacement has matched by now, so the writes are the only step that can fail
    // partway. Reading completely before writing is what keeps that window small.
    //
    // One display for the whole call, each file introduced by its own line: the transcript
    // collapses a tool result to a few rows, and a separate block per file would push the
    // later files out of the preview.
    let mut body = String::new();
    let (mut added, mut removed) = (0usize, 0usize);
    for path in &order {
        let (after, before) = &pending[path];
        tokio::fs::write(path, after)
            .await
            .map_err(|err| format!("无法写入 {}：{err}", path.display()))?;
        observed.forget(path);
        let crate::tools::Display::Diff {
            diff,
            added: a,
            removed: r,
        } = diff::for_edit(before, after)
        else {
            continue;
        };
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("── {}\n", path.display()));
        body.push_str(&diff);
        added += a;
        removed += r;
    }
    Ok(ToolOutput {
        content: format!("已修改 {} 个文件（替换 {replaced_total} 处）", order.len()),
        display: crate::tools::Display::Diff {
            diff: body,
            added,
            removed,
        },
        is_error: false,
        duration: None,
        images: Vec::new(),
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

    /// Every test here edits a file it just wrote, so the staleness record is not the subject
    /// unless the test says so; the ones that do use `Observed` directly.
    async fn run(
        arguments: &serde_json::Value,
        cwd: &Path,
    ) -> Result<crate::tools::ToolOutput, String> {
        execute(arguments, cwd, &crate::tools::observed::Observed::default()).await
    }

    fn file(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn a_file_changed_after_the_read_reports_staleness_not_a_miss() {
        // The reader and the editor share one `Observed` per batch, exactly as the loop wires
        // them. Without that, a model editing from a stale read retries with other spellings
        // of text that is gone, because "not found" reads as its own mistake.
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "a.txt", "alpha\nbeta\n");
        let observed = crate::tools::observed::Observed::default();
        super::super::read::execute(&serde_json::json!({"path": path.clone()}), &dir, &observed)
            .await
            .unwrap();
        std::fs::write(&path, "alpha\nBETA CHANGED\n").unwrap();
        let error = execute(
            &serde_json::json!({"path": path, "old_text": "beta", "new_text": "BETA"}),
            &dir,
            &observed,
        )
        .await
        .unwrap_err();
        assert!(error.contains("被改过"), "{error}");
        assert!(error.contains("重新读"), "{error}");
        assert!(!error.contains("找不到"), "not a miss: {error}");
    }

    #[tokio::test]
    async fn an_edit_that_was_not_preceded_by_a_read_still_works() {
        // Resumed sessions and long turns edit files read many turns ago, where the record no
        // longer exists. Refusing those would break the common case to catch a rarer one.
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "a.txt", "one\n");
        let out = run(
            &serde_json::json!({"path": path, "old_text": "one", "new_text": "two"}),
            &dir,
        )
        .await
        .unwrap();
        assert!(out.content.contains("替换 1 处"));
    }

    #[test]
    fn replaces_a_unique_occurrence() {
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "a.txt", "one\ntwo\nthree\n");
        let out = block(run(
            &serde_json::json!({"path": path, "old_text": "two", "new_text": "TWO"}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("替换 1 处"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
    }

    #[test]
    fn ambiguous_matches_are_refused_unless_replace_all() {
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "b.txt", "x\nx\n");
        let error = block(run(
            &serde_json::json!({"path": path, "old_text": "x", "new_text": "y"}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("出现了 2 次"));
        block(run(
            &serde_json::json!({"path": path, "old_text": "x", "new_text": "y", "replace_all": true}),
            &dir,
        ))
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\ny\n");
    }

    #[test]
    fn a_missing_match_says_what_to_do() {
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "c.txt", "hello\n");
        let error = block(run(
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
        let dir = crate::tools::temp_dir("edit");
        let path = file(
            &dir,
            "drift.txt",
            "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n",
        );
        let error = block(run(
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
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "other.txt", "alpha\nbeta\ngamma\n");
        let error = block(run(
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
        let dir = crate::tools::temp_dir("edit");
        let path = file(&dir, "d.txt", "same\n");
        let error = block(run(
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

    fn batch_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-batch-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The shape a scripted rewrite used to take: 206 of the recorded edits were a `python`
    /// heredoc applying a table of `(path, old, new)`, because `edit` could only change one
    /// place at a time. Files here span more than one, which is the case that forced it.
    #[test]
    fn a_batch_spans_files_and_reports_every_change() {
        let dir = batch_dir();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("b.txt"), "alpha\nbeta\n").unwrap();
        let out = block(run(
            &serde_json::json!({"edits": [
                {"path": "a.txt", "old_text": "two", "new_text": "TWO"},
                {"path": "b.txt", "old_text": "beta", "new_text": "BETA"},
            ]}),
            &dir,
        ))
        .unwrap();
        assert!(out.content.contains("2 个文件"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "alpha\nBETA\n"
        );
        // One block, each file introduced by its own line: the transcript collapses a result
        // to a few rows, and a separate block per file would push the later ones out of it.
        let crate::tools::Display::Diff {
            diff,
            added,
            removed,
        } = &out.display
        else {
            panic!("a batch has to show what it changed");
        };
        assert!(diff.contains("a.txt") && diff.contains("b.txt"), "{diff}");
        assert_eq!((*added, *removed), (2, 2), "{diff}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_batch_that_fails_writes_nothing() {
        // The caller asked for a set of changes, not for as many as happen to apply. A batch
        // that stopped halfway would leave a state neither side can reason about: no way to
        // tell which edits landed, and the file pair no longer meant to be read together.
        let dir = batch_dir();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "alpha\n").unwrap();
        let error = block(run(
            &serde_json::json!({"edits": [
                {"path": "a.txt", "old_text": "one", "new_text": "ONE"},
                {"path": "b.txt", "old_text": "not-here", "new_text": "X"},
            ]}),
            &dir,
        ))
        .unwrap_err();
        // The message names which item failed, so the caller can fix that one.
        assert!(error.contains("第 2 项"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\n",
            "the first edit must not have been written"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("b.txt")).unwrap(),
            "alpha\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edits_in_the_same_file_apply_in_order() {
        // Two edits to one file see each other, which is what makes a batch expressible as a
        // sequence rather than as a set that has to be non-overlapping.
        let dir = batch_dir();
        std::fs::write(dir.join("c.txt"), "a b c\n").unwrap();
        let out = block(run(
            &serde_json::json!({"edits": [
                {"path": "c.txt", "old_text": "a", "new_text": "AA"},
                {"path": "c.txt", "old_text": "c", "new_text": "CC"},
            ]}),
            &dir,
        ))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("c.txt")).unwrap(),
            "AA b CC\n"
        );
        // One file in, one file written: the count is files, not items.
        assert!(out.content.contains("1 个文件"), "{}", out.content);
        assert!(out.content.contains("2 处"), "{}", out.content);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_batch_keeps_the_uniqueness_check_each_single_edit_has() {
        // The check a scripted rewrite skipped. Without it an ambiguous `old_text` silently
        // rewrites the wrong occurrence, and the caller never learns.
        let dir = batch_dir();
        std::fs::write(dir.join("d.txt"), "x x\n").unwrap();
        let error = block(run(
            &serde_json::json!({"edits": [
                {"path": "d.txt", "old_text": "x", "new_text": "y"},
            ]}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("出现了 2 次"), "{error}");
        assert_eq!(std::fs::read_to_string(dir.join("d.txt")).unwrap(), "x x\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_or_malformed_batch_is_refused() {
        let dir = batch_dir();
        std::fs::write(dir.join("e.txt"), "text\n").unwrap();
        for (arguments, expected) in [
            (serde_json::json!({"edits": []}), "edits 是空的"),
            (
                serde_json::json!({"edits": [{"path": "e.txt", "new_text": "x"}]}),
                "缺少 old_text",
            ),
            (serde_json::json!({"edits": "nope"}), "edits 必须是数组"),
        ] {
            let error = block(run(&arguments, &dir)).unwrap_err();
            assert!(error.contains(expected), "{arguments}: {error}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("e.txt")).unwrap(),
            "text\n"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The four levels, each on the drift it exists to absorb.
    ///
    /// These are the shapes in the recorded sessions' misses: a block re-indented, a trailing
    /// space the caller could not see, a placeholder that came back from a chat box as curly
    /// quotes. Refusing them costs a round trip — the caller re-reads the file and tries
    /// again — which is what made `找不到 old_text` the most common tool failure in the log.
    #[test]
    fn a_drifted_match_is_located_and_the_file_keeps_its_own_bytes() {
        // Indentation: the caller indented to 8 where the file has 4. The words are what it
        // meant; the columns are the file's, so the edit lands where the line really sits.
        let before = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let edit = Edit {
            path: "x",
            old_text: "        let a = 1;\n        let b = 2;",
            new_text: "        let a = 9;\n        let b = 2;",
            replace_all: false,
        };
        let (after, replaced) = replace_in(before, &edit).expect("indentation drift");
        assert_eq!(replaced, 1);
        assert_eq!(after, "fn main() {\n    let a = 9;\n    let b = 2;\n}\n");

        // Trailing whitespace the caller could not see.
        let before = "value = 1;   \nother = 2;\t\n";
        let edit = Edit {
            path: "x",
            old_text: "value = 1;\nother = 2;",
            new_text: "value = 9;\nother = 2;",
            replace_all: false,
        };
        let (after, _) = replace_in(before, &edit).expect("trailing whitespace");
        assert_eq!(after, "value = 9;\nother = 2;\t\n");

        // Typographic punctuation: an em dash where the file has a hyphen.
        let before = "// 注释—破折号\nlet x = 1;\n";
        let edit = Edit {
            path: "x",
            old_text: "// 注释-破折号",
            new_text: "// 改过了",
            replace_all: false,
        };
        let (after, _) = replace_in(before, &edit).expect("punctuation");
        assert_eq!(after, "// 改过了\nlet x = 1;\n");
    }

    #[test]
    fn a_fuzzy_match_still_refuses_an_ambiguous_or_absent_one() {
        // Two places that differ only in indentation are still two places, and picking one
        // would rewrite code the caller never looked at.
        let before = "  dup\ndup\n";
        let edit = Edit {
            path: "x",
            old_text: "dup",
            new_text: "x",
            replace_all: false,
        };
        let error = replace_in(before, &edit).unwrap_err();
        assert!(error.contains("出现了 2 次"), "{error}");

        // Silence about which one is the whole point of the uniqueness rule, and loosening
        // the comparison must not loosen it.
        let before = "alpha\nbeta\n";
        let edit = Edit {
            path: "x",
            old_text: "完全不相干的文本",
            new_text: "y",
            replace_all: false,
        };
        assert!(replace_in(before, &edit).is_err());
    }

    #[test]
    fn only_the_changed_lines_are_rewritten() {
        // The region matched loosely, so its unchanged lines must come from the file: taking
        // them from the caller's text would write its drifted indentation into a file that
        // was already correct.
        let before = "fn main() {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n}\n";
        let edit = Edit {
            path: "x",
            old_text: "    let a = 1;\n    let b = 2;\n    let c = 3;",
            new_text: "    let a = 1;\n    let b = 99;\n    let c = 3;",
            replace_all: false,
        };
        let (after, _) = replace_in(before, &edit).unwrap();
        assert_eq!(
            after, "fn main() {\n        let a = 1;\n        let b = 99;\n        let c = 3;\n}\n",
            "the untouched lines must keep the file's own indentation"
        );
    }

    #[test]
    fn a_fuzzy_match_keeps_the_line_ending_it_replaced() {
        // The caller's `new_text` carries no terminator on its last line. Using it verbatim
        // joins the replacement to the next line, which is how a loose match corrupts a file
        // a strict match would have refused.
        let before = "one\ntwo\nthree\n";
        let edit = Edit {
            path: "x",
            old_text: "  two", // drifted: no file line has these columns
            new_text: "TWO",
            replace_all: false,
        };
        let (after, _) = replace_in(before, &edit).unwrap();
        assert_eq!(after, "one\nTWO\nthree\n");

        // A region at end-of-file with no newline stays without one.
        let before = "a\nb";
        let edit = Edit {
            path: "x",
            old_text: "b",
            new_text: "B",
            replace_all: false,
        };
        assert_eq!(replace_in(before, &edit).unwrap().0, "a\nB");
    }

    #[test]
    fn an_exact_match_is_still_exact() {
        // The looser paths must not change what an ordinary edit produces: same bytes in,
        // same bytes out.
        let before = "alpha\nbeta\ngamma\n";
        let edit = Edit {
            path: "x",
            old_text: "beta",
            new_text: "BETA",
            replace_all: false,
        };
        let (after, replaced) = replace_in(before, &edit).unwrap();
        assert_eq!((after.as_str(), replaced), ("alpha\nBETA\ngamma\n", 1));
    }
    /// The shape a recorded session actually sent: `path` once at the top, items without it.
    ///
    /// Requiring `path` per item made the most natural way to write "three edits to one file"
    /// fail, and the session gave up on the tool and went back to `python` heredocs — the path
    /// with no uniqueness check at all.
    #[test]
    fn the_top_level_path_stands_in_for_items_without_one() {
        let dir = crate::tools::temp_dir("edit-default-path");
        std::fs::write(dir.join("m.rs"), "alpha\nbeta\ngamma\n").unwrap();
        let out = block(run(
            &serde_json::json!({
                "path": "m.rs",
                "edits": [
                    {"old_text": "alpha", "new_text": "ALPHA"},
                    {"old_text": "gamma", "new_text": "GAMMA"},
                ]
            }),
            &dir,
        ))
        .expect("the top-level path is the default");
        assert_eq!(
            std::fs::read_to_string(dir.join("m.rs")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
        assert!(out.content.contains("1 个文件"), "{}", out.content);

        // An item that names its own path still wins, so a batch across files is unchanged.
        std::fs::write(dir.join("other.rs"), "one\n").unwrap();
        block(run(
            &serde_json::json!({
                "path": "m.rs",
                "edits": [
                    {"old_text": "beta", "new_text": "BETA"},
                    {"path": "other.rs", "old_text": "one", "new_text": "ONE"},
                ]
            }),
            &dir,
        ))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("m.rs")).unwrap(),
            "ALPHA\nBETA\nGAMMA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("other.rs")).unwrap(),
            "ONE\n"
        );

        // Neither the item nor the top level names a file: say what to do about it.
        let error = block(run(
            &serde_json::json!({"edits": [{"old_text": "x", "new_text": "y"}]}),
            &dir,
        ))
        .unwrap_err();
        assert!(error.contains("顶层也没有给出默认 path"), "{error}");
    }
}
