//! Reading a shell command as literal words, and nothing more.
//!
//! The subset is deliberately tiny: quotes, escapes, `&&`/`||`/`;`/`|` separators, and
//! plain words. Anything whose meaning depends on evaluation — `$(…)`, backticks, `*`,
//! background jobs — is a parse error rather than a guess. A command pi cannot read is a
//! command it asks the user about.
//!
//! Redirections are the one exception, because the common ones do not touch a file at all.
//! `2>&1` and `2>/dev/null` move or discard a file descriptor; they appear in a large share
//! of the commands a model writes by habit, and refusing them teaches the model to reach
//! for something worse — the refusal of `cargo test 2>&1` in practice produced a
//! `python3 - <<EOF`, which is a script that can do anything. So the descriptor forms are
//! parsed and passed through, and only a redirection that names a real file is a refusal.
//!
//! The dialect matters here: zsh expands `=cmd` and `~+` where bash leaves them literal, so
//! a word whose meaning differs from its text must not be rewritten into something else.

/// One parsed word. zsh decides some expansions from the *source* text rather than the
/// resulting value: a leading `=` or `~` is expanded only when it was not quoted or
/// escaped, so `quoted` records whether a quote or escape produced the first character.
/// `''=ls` stays unquoted because an empty quote does not start the word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub value: String,
    pub quoted: bool,
}

/// A command together with the operator that separated it from the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<Word>,
    pub operator: Option<String>,
    /// Redirections that only move or discard a file descriptor, kept in the form they
    /// were written so the command that runs is the command that was checked.
    ///
    /// Only descriptor forms land here — [`descriptor_redirect`] returns nothing for a
    /// redirection that names a file, and the parser refuses those.
    pub redirects: Vec<String>,
}

/// Parse a redirection that does not touch the filesystem, returning the text to put back
/// into the command and the index just past it. `None` means it names a real file (or is
/// malformed), which the caller refuses.
///
/// `2>&1` duplicates a descriptor and `2>/dev/null` writes to the discard device: neither
/// can create, truncate or leak anything a reader would care about. `fd` is the descriptor
/// number written before the operator, empty when the command did not name one.
fn descriptor_redirect(chars: &[char], from: usize, fd: &str) -> Option<(usize, String)> {
    let op = *chars.get(from)?;
    let mut i = from + 1;
    // `2>&1`, `>&2`, `2>&-`, and the input equivalents: a descriptor is named, not a path.
    if chars.get(i) == Some(&'&') {
        let mut j = i + 1;
        let start = j;
        while chars.get(j).is_some_and(char::is_ascii_digit) {
            j += 1;
        }
        if j > start {
            let target: String = chars[start..j].iter().collect();
            return Some((j, format!("{fd}{op}&{target}")));
        }
        if chars.get(j) == Some(&'-') {
            return Some((j + 1, format!("{fd}{op}&-")));
        }
        // `>& file` sends both streams to a file: that is a write.
        return None;
    }
    // Appending is still writing, so only the discard device is accepted either way.
    let append = op == '>' && chars.get(i) == Some(&'>');
    if append {
        i += 1;
    }
    while chars.get(i) == Some(&' ') {
        i += 1;
    }
    let start = i;
    while chars
        .get(i)
        .is_some_and(|c| !c.is_whitespace() && !matches!(c, '<' | '>' | '|' | '&' | ';'))
    {
        i += 1;
    }
    let target: String = chars[start..i].iter().collect();
    if target == "/dev/null" {
        let arrows = if append { ">>" } else { "" };
        return Some((i, format!("{fd}{op}{arrows}/dev/null")));
    }
    None
}

/// Recognise a small literal-shell subset. Everything else returns `Err(reason)` so the
/// caller asks the user. Nothing unrecognised is ever assumed safe.
// The final `flush_segment!` resets the word state one last time; nothing reads it after
// that, which is correct but not something the compiler can see through a macro.
#[allow(unused_assignments)]
pub fn parse_literal_commands(command: &str) -> Result<Vec<Segment>, String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut redirects: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted_start = false;
    let mut quote: Option<char> = None;
    let mut index = 0usize;

    /// Push the pending word and reset the word state. The macro is only ever used
    /// through `flush_segment!`, whose return value is what marks the state as consumed.
    macro_rules! flush_word {
        () => {{
            if started {
                words.push(Word { value: std::mem::take(&mut word), quoted: quoted_start });
            }
            word.clear();
            started = false;
            quoted_start = false;
        }};
    }
    macro_rules! flush_segment {
        ($operator:expr) => {{
            flush_word!();
            if words.is_empty() {
                false
            } else {
                segments.push(Segment {
                    words: std::mem::take(&mut words),
                    operator: $operator,
                    redirects: std::mem::take(&mut redirects),
                });
                true
            }
        }};
    }

    while index < chars.len() {
        let c = chars[index];
        if c == '\0' || (c.is_control() && c != '\n' && c != '\t' && c != '\r') {
            return Err("命令含控制字符".into());
        }
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                if word.is_empty() {
                    quoted_start = true;
                }
                word.push(c);
            }
            index += 1;
            continue;
        }
        if c == '$' || c == '`' {
            return Err("命令含变量、替换或动态 shell 表达式".into());
        }
        if quote == Some('"') {
            if c == '"' {
                quote = None;
                index += 1;
                continue;
            }
            if c == '\\' {
                index += 1;
                let Some(next) = chars.get(index).copied() else {
                    return Err("命令转义不完整".into());
                };
                if next != '\n' {
                    if word.is_empty() {
                        quoted_start = true;
                    }
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        word.push(next);
                    } else {
                        word.push('\\');
                        word.push(next);
                    }
                }
                index += 1;
                continue;
            }
            if word.is_empty() {
                quoted_start = true;
            }
            word.push(c);
            index += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                // Opening a quote keeps an empty argument alive. The protection flag is
                // set later, once a character really lands in the word.
                quote = Some(c);
                started = true;
                index += 1;
            }
            '\\' => {
                index += 1;
                let Some(next) = chars.get(index).copied() else {
                    return Err("命令转义不完整".into());
                };
                if next != '\n' {
                    word.push(next);
                    started = true;
                    quoted_start = true;
                }
                index += 1;
            }
            '#' if !started => {
                while index + 1 < chars.len() && chars[index + 1] != '\n' {
                    index += 1;
                }
                index += 1;
            }
            ' ' | '\t' | '\r' => {
                flush_word!();
                index += 1;
            }
            '\n' | ';' | '|' | '&' => {
                let operator = match c {
                    '\n' | ';' => ";",
                    '|' => {
                        if chars.get(index + 1) == Some(&'|') {
                            index += 1;
                            "||"
                        } else {
                            "|"
                        }
                    }
                    // A single `&` backgrounds the command, which pi never auto-approves.
                    _ => {
                        if chars.get(index + 1) == Some(&'&') {
                            index += 1;
                            "&&"
                        } else {
                            return Err("后台命令需要确认".into());
                        }
                    }
                };
                let had = flush_segment!(Some(operator.to_string()));
                if !had && (c != '\n' || segments.is_empty()) {
                    return Err("无法可靠解析复合命令".into());
                }
                index += 1;
            }
            '<' | '>' => {
                // A descriptor number is written straight against the operator (`2>&1`),
                // so the word the scanner is holding is either that number or nothing.
                // Anything else — `echo a>f` — names a file, and is refused.
                let fd = std::mem::take(&mut word);
                if !fd.is_empty() && (!fd.chars().all(|c| c.is_ascii_digit()) || !started) {
                    return Err("重定向可能写入文件或执行脚本".into());
                }
                match descriptor_redirect(&chars, index, &fd) {
                    Some((end, text)) => {
                        started = false;
                        quoted_start = false;
                        redirects.push(text);
                        index = end;
                    }
                    None => return Err("重定向可能写入文件或执行脚本".into()),
                }
            }
            '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' => {
                return Err("通配符或复合 shell 语法需要确认".into());
            }
            other => {
                started = true;
                word.push(other);
                index += 1;
            }
        }
    }
    if quote.is_some() {
        return Err("命令引号未闭合".into());
    }
    let had_final = flush_segment!(None);
    if !had_final
        && let Some(last) = segments.last()
        && last.operator.is_some()
    {
        return Err("复合命令不完整".into());
    }
    if let Some(last) = segments.last_mut() {
        last.operator = None;
    }
    Ok(segments)
}

/// True when a `sed` script is nothing but a line-range print, as in `sed -n '1,10p' file`.
///
/// `sed 's/a/b/'` and friends rewrite their input rather than showing it, which is a different
/// kind of operation from reading a file, so only the print form is recognised here.
pub(crate) fn is_line_range_print(script: &str) -> bool {
    let Some(body) = script.strip_suffix('p') else { return false };
    if body.is_empty() {
        return true; // bare `p`
    }
    let (start, end) = match body.split_once(',') {
        Some((start, end)) => (start, Some(end)),
        None => (body, None),
    };
    let valid_address = |address: &str| {
        !address.is_empty() && (address == "$" || address.chars().all(|c| c.is_ascii_digit()))
    };
    if !valid_address(start) {
        return false;
    }
    match end {
        Some(end) => valid_address(end),
        None => true,
    }
}

