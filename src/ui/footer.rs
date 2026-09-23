//! The two-line footer.
//!
//! ```text
//! ~/文档/pi-custom (main) • 会话示例
//! ↑ 173k   ↓ 173k   󱘲 99.5%   17.3k/1M                          deepseek-v4.1-flash • high
//! ```
//!
//! Line 1 is the working directory (home shortened to `~`), the git branch and the session
//! name. Line 2 is the four usage fields on the left and the model name right-aligned.
//! The three usage fields are separated by exactly three spaces, and the model keeps at
//! least two columns of clearance from them.

use std::path::Path;

use crate::config::{Defaults, ModelConfig, Usage};
use crate::ui::screen::{Line, Span, Style};
use crate::ui::theme::{CACHE_ICON, Color, Theme};
use crate::util;

/// Everything the footer needs, gathered once per render.
pub struct FooterState<'a> {
    pub cwd: &'a Path,
    pub branch: Option<String>,
    pub session_name: Option<String>,
    pub totals: Usage,
    pub cache_hit_rate: Option<f64>,
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub model: Option<&'a ModelConfig>,
    pub level: &'a str,
    /// A compaction is running: the count is not trustworthy yet.
    pub compacting: bool,
    /// Shown while a turn is in flight.
    pub busy: Option<&'a str>,
}

/// Build the two footer lines at `width` columns.
pub fn render(state: &FooterState<'_>, theme: &Theme, width: usize) -> Vec<Line> {
    let mut lines = vec![location_line(state, theme, width)];
    if let Some(note) = state.busy {
        lines.push(Line::new(note, Style::new(Color::Yellow)));
    }
    lines.push(stats_line(state, theme, width));
    lines
}

fn location_line(state: &FooterState<'_>, theme: &Theme, width: usize) -> Line {
    let _ = theme;
    let cwd = util::shorten_home(state.cwd, dirs::home_dir().as_deref());
    let mut spans = vec![Span::new(util::one_line(&cwd), Style::new(Color::Green))];
    if let Some(branch) = &state.branch {
        spans.push(Span::new(" (", Style::new(Color::Dim)));
        spans.push(Span::new(util::one_line(branch), Style::new(Color::Magenta)));
        spans.push(Span::new(")", Style::new(Color::Dim)));
    }
    if let Some(name) = &state.session_name {
        let name = util::one_line(name);
        if !name.is_empty() {
            spans.push(Span::new(" • ", Style::new(Color::Dim)));
            spans.push(Span::new(
                util::truncate(&name, Defaults::SESSION_NAME_WIDTH, "…"),
                Style::new(Color::Text),
            ));
        }
    }
    let _ = width;
    Line::spans(spans)
}

fn stats_line(state: &FooterState<'_>, theme: &Theme, width: usize) -> Line {
    let hit = match state.cache_hit_rate {
        Some(rate) => format!("{rate:.1}%"),
        None => "—".to_string(),
    };
    let context = match state.context_tokens {
        // After a compaction the old count is meaningless, so it reads as unknown rather
        // than as a stale number.
        _ if state.compacting => "?".to_string(),
        Some(tokens) => util::fmt_tokens(tokens, true),
        None => "?".to_string(),
    };
    let window = state
        .context_window
        .map(|window| util::fmt_tokens(window, true))
        .unwrap_or_else(|| "?".to_string());
    let context_text = format!("{context}/{window}");

    let percent = match (state.context_tokens, state.context_window) {
        (Some(tokens), Some(window)) if window > 0 => tokens as f64 / window as f64 * 100.0,
        _ => 0.0,
    };
    let context_color = theme.context(percent);

    let green = Style::new(Color::Green);
    let left = format!(
        "↑ {}   ↓ {}   {CACHE_ICON} {hit}   {context_text}",
        util::fmt_tokens(state.totals.input, false),
        util::fmt_tokens(state.totals.output, false),
    );
    // Split the left half back into its four runs so each context field can carry its own
    // colour: the three usage fields are green, the gauge changes with how full it is.
    let mut spans = vec![
        Span::new(format!("↑ {}", util::fmt_tokens(state.totals.input, false)), green),
        Span::new("   ", Style::plain()),
        Span::new(format!("↓ {}", util::fmt_tokens(state.totals.output, false)), green),
        Span::new("   ", Style::plain()),
        Span::new(format!("{CACHE_ICON} {hit}"), green),
        Span::new("   ", Style::plain()),
        Span::new(context_text.clone(), Style::new(context_color)),
    ];
    let left_width = util::width(&left);

    let mut model = String::new();
    if let Some(model_config) = state.model {
        model = util::one_line(model_config.display_name());
        // A model that cannot reason gets no suffix at all.
        if model_config.reasoning {
            let level = if state.level.is_empty() { "low" } else { state.level };
            model = format!("{model} • {level}");
        }
    }
    // The model keeps at least two columns of clearance from the stats.
    let available = width.saturating_sub(left_width).saturating_sub(2);
    if available > 0 && !model.is_empty() {
        let rendered = if util::width(&model) <= available {
            model
        } else {
            util::truncate(&model, available, "…")
        };
        let gap = width
            .saturating_sub(left_width)
            .saturating_sub(util::width(&rendered));
        spans.push(Span::plain(" ".repeat(gap)));
        spans.push(Span::new(rendered, Style::new(Color::Cyan)));
    }
    Line::spans(spans)
}

/// The git branch for `cwd`, if it is inside a repository.
///
/// Read straight from `.git/HEAD` rather than shelling out to git: this runs on every
/// render, and `git` is not always installed.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        let head = current.join(".git/HEAD");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            let contents = contents.trim();
            if let Some(reference) = contents.strip_prefix("ref: ") {
                let name = reference.rsplit('/').next().unwrap_or(reference);
                return Some(name.to_string());
            }
            if !contents.is_empty() {
                return Some(util::truncate(contents, 12, "…"));
            }
        }
        dir = current.parent();
    }
    None
}

/// Colour for a transcript-level status dot.
pub fn status_style(ok: bool) -> (Style, &'static str) {
    if ok {
        (Style { fg: Color::Green, bold: true, ..Style::plain() }, "✓")
    } else {
        (Style { fg: Color::Red, bold: true, ..Style::plain() }, "×")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::plain;
    use crate::ui::theme::ColorMode;

    fn model() -> ModelConfig {
        serde_json::from_str(r#"{"id":"deepseek-v4.1-flash","name":"deepseek-v4.1-flash","reasoning":true,"context_window":1000000}"#)
            .unwrap()
    }

    fn state(cwd: &str) -> FooterState<'_> {
        FooterState {
            cwd: Path::new(cwd),
            branch: Some("main".into()),
            session_name: Some("会话示例".into()),
            totals: Usage { input: 173_000, output: 173_000, cache_read: 0, cache_write: 0 },
            cache_hit_rate: Some(99.5),
            context_tokens: Some(17_300),
            context_window: Some(1_000_000),
            model: None,
            level: "high",
            compacting: false,
            busy: None,
        }
    }

    #[test]
    fn the_stats_line_matches_the_specified_layout() {
        let theme = Theme { mode: ColorMode::True };
        let model_config = model();
        let mut state = state("/tmp/中文目录");
        state.model = Some(&model_config);
        let lines = render(&state, &theme, 120);
        let text = &plain(&lines)[1];
        assert!(
            text.starts_with("↑ 173k   ↓ 173k   \u{f1632} 99.5%   17.3k/1M"),
            "{text:?}"
        );
        assert!(text.ends_with("deepseek-v4.1-flash • high"), "{text:?}");
        assert_eq!(util::width(text), 120);
        // At least two columns of clearance before the right-aligned model name.
        let gap = text.find("deepseek").unwrap() - text.find("17.3k/1M").unwrap() - "17.3k/1M".len();
        assert!(gap >= 2, "only {gap} columns of gap");
    }

    #[test]
    fn the_location_line_shows_directory_branch_and_name() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let home = dirs::home_dir().unwrap();
        let nested = home.join("文档/pi-custom");
        let nested_text = nested.to_string_lossy().to_string();
        let state = state(&nested_text);
        let lines = render(&state, &theme, 120);
        let text = &plain(&lines)[0];
        assert!(text.starts_with("~/文档/pi-custom"), "{text:?}");
        assert!(text.contains("(main)"));
        assert!(text.ends_with("• 会话示例"), "{text:?}");
    }

    #[test]
    fn a_non_reasoning_model_gets_no_level_suffix() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let plain_model: ModelConfig =
            serde_json::from_str(r#"{"id":"plain-model","reasoning":false}"#).unwrap();
        let mut state = state("/tmp");
        state.model = Some(&plain_model);
        let lines = render(&state, &theme, 120);
        assert!(plain(&lines)[1].ends_with("plain-model"));
        assert!(!plain(&lines)[1].contains("•"));
    }

    #[test]
    fn an_unknown_context_window_renders_as_a_question_mark() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let mut state = state("/tmp");
        state.context_window = None;
        state.context_tokens = None;
        assert!(plain(&render(&state, &theme, 120))[1].contains("?/?"));
    }

    #[test]
    fn a_running_compaction_hides_the_stale_token_count() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let mut state = state("/tmp");
        state.compacting = true;
        let text = plain(&render(&state, &theme, 120))[1].clone();
        assert!(text.contains("?/1M"), "{text:?}");
        assert!(!text.contains("17.3k"), "{text:?}");
    }

    #[test]
    fn the_context_field_warns_and_then_errors() {
        let theme = Theme { mode: ColorMode::True };
        let mut state = state("/tmp");
        let gauge = |state: &FooterState<'_>| {
            let line = render(state, &theme, 120)[1].clone();
            let span = line.spans.iter().find(|span| span.text.contains('/')).unwrap().clone();
            (span.text, span.style.fg)
        };
        state.context_tokens = Some(750_000);
        assert_eq!(gauge(&state), ("750k/1M".to_string(), Color::Yellow));
        state.context_tokens = Some(950_000);
        assert_eq!(gauge(&state), ("950k/1M".to_string(), Color::Red));
        state.context_tokens = Some(17_300);
        assert_eq!(gauge(&state), ("17.3k/1M".to_string(), Color::Green));
    }

    #[test]
    fn no_cache_data_reads_as_a_dash() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let mut state = state("/tmp");
        state.cache_hit_rate = None;
        assert!(plain(&render(&state, &theme, 120))[1].contains("\u{f1632} —"));
    }

    #[test]
    fn the_session_name_is_truncated_to_the_configured_width() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let long = "名".repeat(100);
        let mut state = state("/tmp");
        state.session_name = Some(long);
        let text = plain(&render(&state, &theme, 200))[0].clone();
        let name = text.split("• ").nth(1).unwrap();
        assert!(util::width(name) <= Defaults::SESSION_NAME_WIDTH + 1, "{name:?}");
    }

    #[test]
    fn the_branch_is_read_from_head_without_running_git() {
        let dir = std::env::temp_dir().join(format!("pi-footer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("x"));
        // A subdirectory still finds the repository root.
        let nested = dir.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_branch(&nested).as_deref(), Some("x"));
        // A detached HEAD shows the short hash.
        std::fs::write(dir.join(".git/HEAD"), "0123456789abcdef0123\n").unwrap();
        assert_eq!(git_branch(&dir).as_deref(), Some("0123456789a…"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_busy_note_only_adds_a_row_when_present() {
        let theme = Theme { mode: ColorMode::Ansi256 };
        let mut state = state("/tmp");
        assert_eq!(render(&state, &theme, 120).len(), 2);
        state.busy = Some("等待用户授权");
        assert_eq!(render(&state, &theme, 120).len(), 3);
    }
}
