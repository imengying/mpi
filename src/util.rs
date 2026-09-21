//! Small text/path helpers shared by the UI and the tools.
//!
//! Everything here is display-layer: no function in this module is allowed to change
//! what the model sees, except `truncate_output`, which is documented as such.

use std::path::{Path, PathBuf};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Scale a count down to `k` / `M`, dropping a trailing `.0` so whole values read as
/// `256k` / `1M` while fractional ones keep one decimal (`17.3k`).
pub fn fmt_tokens(count: u64, precise: bool) -> String {
    let scale = |n: u64, div: f64, suffix: &str| {
        let text = format!("{:.1}", n as f64 / div);
        let text = text.strip_suffix(".0").unwrap_or(&text).to_string();
        format!("{text}{suffix}")
    };
    if count < 1000 {
        return count.to_string();
    }
    if count < 1_000_000 {
        return if precise || count < 10_000 {
            scale(count, 1000.0, "k")
        } else {
            format!("{}k", (count as f64 / 1000.0).round() as u64)
        };
    }
    if count < 10_000_000 {
        scale(count, 1_000_000.0, "M")
    } else {
        format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
    }
}

/// Remove terminal control sequences. Used to neuter untrusted text (command output,
/// model output) before it reaches the terminal.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters then a final byte in @..~
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / SOS / PM / APC: run until BEL or ST.
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-character escape (e.g. `\e(0`).
                Some(_) | None => {}
            },
            '\r' => {}
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {}
            c if ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Make text safe to print *without hiding anything*.
///
/// Unlike [`sanitize`], nothing is dropped: every control character, escape sequence and
/// bidi override is rendered as visible `\uXXXX` text. The authorization panel uses this
/// because the user is approving exactly what they see — if the displayed command were
/// prettified, they could approve one thing and run another.
pub fn review(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for c in normalized.chars() {
        let code = c as u32;
        if c == '\t' {
            out.push_str("    ");
        } else if c == '\n' {
            // A newline is structural, not content: a multi-line command stays multi-line
            // and only the wrapping decides where the rows break.
            out.push(c);
        } else if code < 0x20 || code == 0x7f || (0x80..=0x9f).contains(&code) || is_bidi(c) {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            out.push(c);
        }
    }
    out
}

/// Bidi and directional controls, which can reorder what a human reads.
fn is_bidi(c: char) -> bool {
    matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}')
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// Make text safe to print: strip escapes, normalize line endings, and render
/// remaining control characters (and bidi overrides) visibly.
pub fn sanitize(input: &str) -> String {
    let stripped = strip_ansi(&input.replace("\r\n", "\n"));
    let mut out = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        let code = c as u32;
        if (code < 0x20 && c != '\n' && c != '\t') || code == 0x7f || is_bidi(c) {
            out.push_str(&format!("\\u{code:04x}"));
        } else if c == '\t' {
            out.push_str("    ");
        } else {
            out.push(c);
        }
    }
    out
}

/// Collapse to a single printable line.
pub fn one_line(input: &str) -> String {
    let text = sanitize(input);
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

pub fn width(s: &str) -> usize {
    s.width()
}

/// Truncate to `max` display columns, appending `ellipsis` when cut.
pub fn truncate(s: &str, max: usize, ellipsis: &str) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(ellipsis.width());
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > keep {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push_str(ellipsis);
    out
}

/// Pad to `cols` display columns (truncating first if necessary).
pub fn pad(s: &str, cols: usize) -> String {
    let text = truncate(s, cols, "");
    let used = text.width();
    format!("{text}{}", " ".repeat(cols.saturating_sub(used)))
}

/// Soft-wrap to `cols` columns on display width. Never breaks inside a word unless the
/// word alone is wider than the line.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut used = 0usize;
        for word in raw.split(' ') {
            let ww = word.width();
            if used > 0 && used + 1 + ww > cols {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            if ww > cols {
                // Break the oversized word by character.
                if used > 0 {
                    lines.push(std::mem::take(&mut current));
                }
                let mut chunk = String::new();
                let mut chunk_w = 0;
                for c in word.chars() {
                    let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                    if chunk_w + cw > cols {
                        lines.push(std::mem::take(&mut chunk));
                        chunk_w = 0;
                    }
                    chunk.push(c);
                    chunk_w += cw;
                }
                current = chunk;
                used = chunk_w;
                continue;
            }
            if used > 0 {
                current.push(' ');
                used += 1;
            }
            current.push_str(word);
            used += ww;
        }
        lines.push(current);
    }
    lines
}

/// Replace the home directory prefix with `~`.
pub fn shorten_home(path: &Path, home: Option<&Path>) -> String {
    let text = path.to_string_lossy().to_string();
    let Some(home) = home else { return text };
    let home = home.to_string_lossy();
    if text == home {
        return "~".into();
    }
    match text.strip_prefix(home.as_ref()) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => text,
    }
}

/// Where the full (untruncated) tool output is parked.
pub fn temp_output_path() -> PathBuf {
    let mut counter = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    counter &= 0xffff;
    std::env::temp_dir().join(format!("pi-{pid}-{stamp:x}-{counter:x}.log"))
}

static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Output truncation budget: 2000 lines or 50KB, whichever is reached first.
pub const MAX_OUTPUT_LINES: usize = 2000;
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Result of applying the tool-output budget.
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub dropped_lines: usize,
    pub full_path: Option<PathBuf>,
}

/// Keep the **tail** of an oversized result and park the whole thing in a temp file,
/// whose path is appended for the model to follow up on.
pub fn truncate_output(raw: &str) -> Truncated {
    let lines: Vec<&str> = raw.split('\n').collect();
    let over_lines = lines.len().saturating_sub(MAX_OUTPUT_LINES);
    let over_bytes = raw.len().saturating_sub(MAX_OUTPUT_BYTES);
    if over_lines == 0 && over_bytes == 0 {
        return Truncated {
            text: raw.to_string(),
            truncated: false,
            dropped_lines: 0,
            full_path: None,
        };
    }
    let full_path = temp_output_path();
    if let Err(err) = std::fs::write(&full_path, raw) {
        // Losing the temp copy is better than losing the answer: report it inline.
        let kept = tail(raw, &lines);
        return Truncated {
            text: format!("{kept}\n\n[完整输出写入临时文件失败：{err}]"),
            truncated: true,
            dropped_lines: over_lines,
            full_path: None,
        };
    }
    let kept = tail(raw, &lines);
    let note = format!(
        "\n\n[输出过长，已截断：保留末尾 {} 行 / {} 字节。完整输出：{}]",
        kept.lines().count(),
        kept.len(),
        full_path.display()
    );
    Truncated {
        text: format!("{kept}{note}"),
        truncated: true,
        dropped_lines: over_lines,
        full_path: Some(full_path),
    }
}

fn tail(raw: &str, lines: &[&str]) -> String {
    let start_line = lines.len().saturating_sub(MAX_OUTPUT_LINES);
    let candidate = lines[start_line..].join("\n");
    if candidate.len() <= MAX_OUTPUT_BYTES {
        return candidate;
    }
    // Cut on a line boundary inside the byte budget so the model never sees a
    // half-decoded character.
    let mut offset = candidate.len() - MAX_OUTPUT_BYTES;
    while offset < candidate.len() && !candidate.is_char_boundary(offset) {
        offset += 1;
    }
    let body = &candidate[offset..];
    let _ = raw;
    match body.find('\n') {
        Some(idx) if idx + 1 < body.len() => body[idx + 1..].to_string(),
        _ => body.to_string(),
    }
}

/// Estimated token count for a text blob (chars / 4).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_formatting_matches_the_footer_spec() {
        assert_eq!(fmt_tokens(999, true), "999");
        assert_eq!(fmt_tokens(17_300, true), "17.3k");
        assert_eq!(fmt_tokens(256_000, true), "256k");
        assert_eq!(fmt_tokens(1_000_000, true), "1M");
        assert_eq!(fmt_tokens(256_500, true), "256.5k");
    }

    #[test]
    fn review_escapes_instead_of_dropping() {
        // Nothing may disappear: the panel's whole job is to show what will run.
        assert_eq!(review("a\u{7}b"), "a\\u0007b");
        assert_eq!(review("\u{1b}[31mred"), "\\u001b[31mred");
        assert_eq!(review("x\u{202e}y"), "x\\u202ey");
        assert_eq!(review("tab\there"), "tab    here");
        assert_eq!(review("plain text"), "plain text");
        // A newline survives so a multi-line command stays multi-line; wrapping handles the
        // layout separately.
        assert_eq!(review("a\nb"), "a\nb");
        // A carriage return is rewritten, not passed through: it would let a payload move
        // the cursor back over the choices.
        assert_eq!(review("a\rb"), "a\nb");
        assert!(review("a\u{b}b").contains("\\u000b"));
    }

    #[test]
    fn sanitize_drops_what_review_would_show() {
        // The two functions differ on purpose: tool output is cleaned, the approval text is
        // made unambiguous.
        assert_eq!(sanitize("a\u{7}b"), "ab");
        assert_eq!(strip_ansi("\u{1b}[31mred"), "red");
    }

    #[test]
    fn ansi_is_removed_without_touching_plain_text() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn wrapping_counts_double_width_cells() {
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("a b c", 3), vec!["a b", "c"]);
    }

    #[test]
    fn tiny_output_is_not_truncated() {
        let out = truncate_output("hello");
        assert!(!out.truncated);
        assert_eq!(out.text, "hello");
    }

    #[test]
    fn huge_output_keeps_the_tail_and_reports_a_path() {
        let raw: String = (0..MAX_OUTPUT_LINES + 10).map(|i| format!("line{i}\n")).collect();
        let out = truncate_output(&raw);
        assert!(out.truncated);
        assert!(out.text.contains("line2009"));
        assert!(!out.text.contains("line0\n"));
        assert!(out.full_path.is_some());
    }
}
