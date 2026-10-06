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
//!
//! The context field carries its own state rather than a bare number: see [`ContextUsage`]
//! for why a session that has said nothing shows `—` instead of counting what the request
//! would carry.

use std::path::Path;

use crate::config::{Defaults, ModelConfig, Usage};
use crate::ui::screen::{Line, Span, Style};
use crate::ui::theme::{CACHE_ICON, Color, Theme};
use crate::util;

/// What the context field knows, which is exactly what it can print.
///
/// Three states rather than a `Option<u64>` plus a flag, because the field prints three
/// different things and each one means something different: `—` before the session has
/// said anything, `?` while a compaction is replacing the context the last count
/// described, and a number when there is a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextUsage {
    /// Nothing to measure yet. A session that has not been spoken to carries only the
    /// environment block, and counting that plus the tool schemas would report the fixed
    /// prefix every request carries as context the user had spent before typing anything.
    Unmeasured,
    /// A count exists and must not be shown: a compaction is running, so the number
    /// describes the context being replaced, not the one the next request will carry.
    Stale,
    /// The tokens the next request would carry.
    Tokens(u64),
}

/// The context state for a session, decided where the states are defined.
///
/// The order is the point: a running compaction hides the count it is replacing, a count
/// the upstream reported beats any estimate, and a session that has said nothing is not
/// estimated at all. `estimate` runs only when its number would be shown, so a fresh
/// session never pays to measure a context that is not there.
pub fn context_usage(
    compacting: bool,
    measured: Option<u64>,
    holds_only_environment: bool,
    estimate: impl FnOnce() -> u64,
) -> ContextUsage {
    if compacting {
        ContextUsage::Stale
    } else if let Some(measured) = measured {
        ContextUsage::Tokens(measured)
    } else if holds_only_environment {
        ContextUsage::Unmeasured
    } else {
        ContextUsage::Tokens(estimate())
    }
}

/// Everything the footer needs, gathered once per render.
#[derive(Clone)]
pub struct FooterState {
    pub cwd: std::path::PathBuf,
    pub branch: Option<String>,
    pub session_name: Option<String>,
    pub totals: Usage,
    pub cache_hit_rate: Option<f64>,
    pub context_usage: ContextUsage,
    pub context_window: Option<u64>,
    pub model: Option<ModelConfig>,
    pub level: String,
    /// Shown while a turn is in flight.
    pub busy: Option<String>,
}

/// Build the two footer lines at `width` columns.
pub fn render(state: &FooterState, theme: &Theme, width: usize) -> Vec<Line> {
    let mut lines = vec![location_line(state, width)];
    if let Some(note) = &state.busy {
        lines.push(Line::new(note, Style::new(Color::Yellow)));
    }
    lines.push(stats_line(state, theme, width));
    lines
}

fn location_line(state: &FooterState, width: usize) -> Line {
    let cwd = util::shorten_home(&state.cwd, dirs::home_dir().as_deref());
    // The line is built to fit `width`: it is one row of the live region, and a row that the
    // terminal wraps on its own would throw the region's row count off by one — which shows up
    // as the whole footer creeping down the screen, a row per redraw.
    //
    // The directory is what the user needs to see, so it is what survives: the branch loses
    // its tail first, then the session name, then the directory itself.
    let name = state
        .session_name
        .as_ref()
        .map(|name| util::one_line(name))
        .filter(|name| !name.is_empty());
    let branch = state.branch.as_ref().map(|branch| util::one_line(branch));
    let mut budget = width;
    let cwd = {
        let shown = util::truncate(&util::one_line(&cwd), budget, "…");
        budget = budget.saturating_sub(util::width(&shown));
        shown
    };
    let mut spans = vec![Span::new(cwd, Style::new(Color::Green))];
    if let Some(branch) = branch {
        // " (" + branch + ")", and at least one column so an empty branch cannot produce "()".
        let room = budget.saturating_sub(3);
        if room > 0 {
            let shown = util::truncate(&branch, room, "…");
            budget = budget.saturating_sub(util::width(&shown) + 3);
            spans.push(Span::new(" (", Style::new(Color::Dim)));
            spans.push(Span::new(shown, Style::new(Color::Magenta)));
            spans.push(Span::new(")", Style::new(Color::Dim)));
        }
    }
    if let Some(name) = name {
        // A session name is the least important of the three: it is the one the user just
        // chose, and the footer still has its own line for the model and level.
        let room = budget.saturating_sub(3).min(Defaults::SESSION_NAME_WIDTH);
        if room > 0 {
            spans.push(Span::new(" • ", Style::new(Color::Dim)));
            spans.push(Span::new(
                util::truncate(&name, room, "…"),
                Style::new(Color::Text),
            ));
        }
    }
    Line::spans(spans)
}

fn stats_line(state: &FooterState, theme: &Theme, width: usize) -> Line {
    let hit = match state.cache_hit_rate {
        Some(rate) => format!("{rate:.1}%"),
        None => "—".to_string(),
    };
    let context = match state.context_usage {
        // A count that describes the context being replaced is not printed: it would name
        // what compaction is throwing away, not what the next request will carry.
        ContextUsage::Stale => "?".to_string(),
        // Nothing has been said yet, so there is nothing spent to report.
        ContextUsage::Unmeasured => "—".to_string(),
        ContextUsage::Tokens(tokens) => util::fmt_tokens(tokens, true),
    };
    let window = state
        .context_window
        .map(|window| util::fmt_tokens(window, true))
        .unwrap_or_else(|| "?".to_string());
    let context_text = format!("{context}/{window}");

    let percent = match (state.context_usage, state.context_window) {
        (ContextUsage::Tokens(tokens), Some(window)) if window > 0 => {
            tokens as f64 / window as f64 * 100.0
        }
        _ => 0.0,
    };
    let context_color = theme.context(percent);

    let usage = Style::new(Color::Green);
    let left = format!(
        "↑ {}   ↓ {}   {CACHE_ICON} {hit}   {context_text}",
        util::fmt_tokens(state.totals.input, false),
        util::fmt_tokens(state.totals.output, false),
    );
    // Split the left half back into its four runs so each context field can carry its own
    // colour: usage keeps its theme accent, while the gauge highlights warnings and errors.
    let mut spans = vec![
        Span::new(
            format!("↑ {}", util::fmt_tokens(state.totals.input, false)),
            usage,
        ),
        Span::new("   ", Style::plain()),
        Span::new(
            format!("↓ {}", util::fmt_tokens(state.totals.output, false)),
            usage,
        ),
        Span::new("   ", Style::plain()),
        Span::new(format!("{CACHE_ICON} {hit}"), usage),
        Span::new("   ", Style::plain()),
        Span::new(context_text.clone(), Style::new(context_color)),
    ];
    let mut left_width = util::width(&left);

    if left_width + 4 > width {
        let context = util::truncate(
            &context_text,
            width.saturating_sub(3).min(width / 2).max(1),
            "…",
        );
        left_width = util::width(&context);
        spans = vec![Span::new(context, Style::new(context_color))];
    }

    let mut model = String::new();
    if let Some(model_config) = &state.model {
        model = util::one_line(model_config.display_name());
        // A model that cannot reason gets no suffix at all.
        if model_config.reasoning {
            let level = if state.level.is_empty() {
                "low"
            } else {
                &state.level
            };
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
        let level = if state.level.is_empty() {
            "low"
        } else {
            &state.level
        };
        let model_color = if state.model.as_ref().is_some_and(|model| model.reasoning) {
            Color::reasoning(level)
        } else {
            Color::Cyan
        };
        let suffix = format!(" • {level}");
        if state.model.as_ref().is_some_and(|model| model.reasoning) && rendered.ends_with(&suffix)
        {
            spans.push(Span::new(
                rendered[..rendered.len() - suffix.len()].to_string(),
                Style::new(model_color),
            ));
            spans.push(Span::new(" • ", Style::new(Color::Dim)));
            spans.push(Span::new(level, Style::new(Color::reasoning(level))));
        } else {
            spans.push(Span::new(rendered, Style::new(model_color)));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::plain;
    use crate::ui::theme::ColorMode;

    #[test]
    fn reasoning_levels_have_distinct_accents_in_the_footer() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let model = model();
        let mut state = state("/tmp");
        state.model = Some(model.clone());
        let mut colors = Vec::new();
        for level in ["low", "medium", "high", "xhigh", "max"] {
            state.level = level.into();
            let rows = render(&state, &theme, 120);
            let color = rows[1]
                .spans
                .iter()
                .find(|s| s.text == level)
                .unwrap()
                .style
                .fg;
            assert!(!colors.contains(&color));
            colors.push(color);
        }
    }

    fn model() -> ModelConfig {
        serde_json::from_str(r#"{"id":"deepseek-v4.1-flash","name":"deepseek-v4.1-flash","reasoning":true,"context_window":1000000}"#)
            .unwrap()
    }

    fn state(cwd: &str) -> FooterState {
        FooterState {
            cwd: cwd.into(),
            branch: Some("main".into()),
            session_name: Some("会话示例".into()),
            totals: Usage {
                input: 173_000,
                output: 173_000,
                cache_read: 0,
                cache_write: 0,
            },
            cache_hit_rate: Some(99.5),
            context_usage: ContextUsage::Tokens(17_300),
            context_window: Some(1_000_000),
            model: None,
            level: "high".into(),
            busy: None,
        }
    }

    #[test]
    fn the_stats_line_matches_the_specified_layout() {
        let theme = Theme {
            mode: ColorMode::True,
        };
        let model_config = model();
        let mut state = state("/tmp/中文目录");
        state.model = Some(model_config);
        let lines = render(&state, &theme, 120);
        let text = &plain(&lines)[1];
        assert!(
            text.starts_with("↑ 173k   ↓ 173k   \u{f1632} 99.5%   17.3k/1M"),
            "{text:?}"
        );
        assert!(text.ends_with("deepseek-v4.1-flash • high"), "{text:?}");
        assert_eq!(util::width(text), 120);
        // At least two columns of clearance before the right-aligned model name.
        let gap =
            text.find("deepseek").unwrap() - text.find("17.3k/1M").unwrap() - "17.3k/1M".len();
        assert!(gap >= 2, "only {gap} columns of gap");
    }

    #[test]
    fn the_location_line_shows_directory_branch_and_name() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
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
    fn the_location_line_never_exceeds_the_width() {
        // This row is part of the live region, and a row the terminal wraps by itself throws
        // the region's row count off — the footer then creeps down the screen, a row per
        // redraw, which reads as the footer repeating itself. So the line is built to fit.
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        state.branch = Some("feature/一个很长的分支名字用来撑破这一行".repeat(3));
        state.session_name = Some("会话名字同样可以很长".repeat(5));

        for width in [20usize, 30, 40, 80, 120] {
            let text = plain(&render(&state, &theme, width))[0].clone();
            assert!(
                util::width(&text) <= width,
                "{width}: {text:?} is {} cells",
                util::width(&text)
            );
        }

        // The directory is what the user needs, so it is what survives the squeeze: the
        // branch and the name give way first.
        let text = plain(&render(&state, &theme, 30))[0].clone();
        assert!(text.starts_with("/tmp"), "{text:?}");
    }

    #[test]
    fn a_non_reasoning_model_gets_no_level_suffix() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let plain_model: ModelConfig =
            serde_json::from_str(r#"{"id":"plain-model","reasoning":false}"#).unwrap();
        let mut state = state("/tmp");
        state.model = Some(plain_model);
        let lines = render(&state, &theme, 120);
        assert!(plain(&lines)[1].ends_with("plain-model"));
        assert!(!plain(&lines)[1].contains("•"));
    }

    #[test]
    fn an_unknown_context_window_renders_as_a_question_mark() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        state.context_window = None;
        assert!(plain(&render(&state, &theme, 120))[1].contains("17.3k/?"));
    }

    #[test]
    fn a_session_that_has_said_nothing_shows_no_context_usage() {
        // What a fresh session would otherwise print is the fixed prefix every request
        // carries — the tool schemas and the environment block, some 1.1k with these seven
        // tools — which is not context the user has spent. `—` says that, and says it the
        // same way the cache field says a session has no cache data yet.
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        state.context_usage = ContextUsage::Unmeasured;
        let text = plain(&render(&state, &theme, 120))[1].clone();
        assert!(text.contains("—/1M"), "{text:?}");
        assert!(!text.contains("0/1M"), "{text:?}");
    }

    #[test]
    fn a_running_compaction_hides_the_stale_token_count() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        state.context_usage = ContextUsage::Stale;
        let text = plain(&render(&state, &theme, 120))[1].clone();
        assert!(text.contains("?/1M"), "{text:?}");
        assert!(!text.contains("17.3k"), "{text:?}");
    }

    #[test]
    fn the_estimate_is_asked_for_only_when_its_number_would_be_shown() {
        use std::cell::Cell;
        let asked = Cell::new(0);
        let estimate = || {
            asked.set(asked.get() + 1);
            1_138
        };
        // A fresh session is never estimated: counting the fixed prefix is what put a
        // number on screen before the user had said anything.
        assert_eq!(
            context_usage(false, None, true, estimate),
            ContextUsage::Unmeasured
        );
        assert_eq!(asked.get(), 0);
        // A reported count beats an estimate, and a running compaction hides both.
        assert_eq!(
            context_usage(false, Some(42), false, estimate),
            ContextUsage::Tokens(42)
        );
        assert_eq!(
            context_usage(true, Some(42), false, estimate),
            ContextUsage::Stale
        );
        assert_eq!(asked.get(), 0);
        // A conversation with no reading yet is the one case that pays for the estimate.
        assert_eq!(
            context_usage(false, None, false, estimate),
            ContextUsage::Tokens(1_138)
        );
        assert_eq!(asked.get(), 1);
    }

    #[test]
    fn the_context_field_warns_and_then_errors() {
        let theme = Theme {
            mode: ColorMode::True,
        };
        let mut state = state("/tmp");
        let gauge = |state: &FooterState| {
            let line = render(state, &theme, 120)[1].clone();
            let span = line
                .spans
                .iter()
                .find(|span| span.text.contains('/'))
                .unwrap()
                .clone();
            (span.text, span.style.fg)
        };
        state.context_usage = ContextUsage::Tokens(750_000);
        assert_eq!(gauge(&state), ("750k/1M".to_string(), Color::Yellow));
        state.context_usage = ContextUsage::Tokens(950_000);
        assert_eq!(gauge(&state), ("950k/1M".to_string(), Color::Red));
        state.context_usage = ContextUsage::Tokens(17_300);
        assert_eq!(gauge(&state), ("17.3k/1M".to_string(), Color::Green));
    }

    #[test]
    fn no_cache_data_reads_as_a_dash() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        state.cache_hit_rate = None;
        assert!(plain(&render(&state, &theme, 120))[1].contains("\u{f1632} —"));
    }

    #[test]
    fn the_session_name_is_truncated_to_the_configured_width() {
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let long = "名".repeat(100);
        let mut state = state("/tmp");
        state.session_name = Some(long);
        let text = plain(&render(&state, &theme, 200))[0].clone();
        let name = text.split("• ").nth(1).unwrap();
        assert!(
            util::width(name) <= Defaults::SESSION_NAME_WIDTH + 1,
            "{name:?}"
        );
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
        let theme = Theme {
            mode: ColorMode::Ansi256,
        };
        let mut state = state("/tmp");
        assert_eq!(render(&state, &theme, 120).len(), 2);
        state.busy = Some("等待用户授权".into());
        assert_eq!(render(&state, &theme, 120).len(), 3);
    }
}
