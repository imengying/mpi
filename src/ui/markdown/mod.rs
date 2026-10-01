//! CommonMark rendering for assistant answers, including summaries and restored sessions.
//! Source remains Markdown; the screen handles terminal layout and native selection.

use crate::ui::text::Line;
use crate::util;
use pulldown_cmark::{Event, Options, Parser, TagEnd};

mod code;
mod render;

pub(super) const OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_STRIKETHROUGH);

pub(super) fn parser(text: &str) -> Parser<'_> {
    Parser::new_ext(text, OPTIONS)
}

/// Render logical lines; the screen preserves source breaks when the terminal wraps them.
pub fn render(text: &str, width: usize) -> Vec<Line> {
    render::render(&util::sanitize(text), width)
}

/// Only commit complete top-level blocks. Blank lines inside lists, quotes and code
/// do not prove those containers have ended. Reference links may be defined later.
pub(crate) fn stable_prefix(text: &str) -> usize {
    let mut unresolved = usize::MAX;
    let mut callback = |link: pulldown_cmark::BrokenLink<'_>| {
        unresolved = unresolved.min(link.span.start);
        None
    };
    let parsed = Parser::new_with_broken_link_callback(text, OPTIONS, Some(&mut callback));
    if parsed.reference_definitions().iter().next().is_some() {
        return 0;
    }
    let mut depth = 0;
    let mut blocks = Vec::new();
    for (event, range) in parsed.into_offset_iter() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(tag) => {
                depth -= 1;
                if depth == 0 {
                    blocks.push((tag, range));
                }
            }
            Event::Rule => blocks.push((TagEnd::Paragraph, range)),
            _ => {}
        }
    }
    let mut stable = 0;
    for (index, (tag, range)) in blocks.iter().enumerate() {
        if range.end > unresolved {
            break;
        }
        if index + 1 == blocks.len()
            && matches!(
                tag,
                TagEnd::List(_) | TagEnd::BlockQuote(_) | TagEnd::CodeBlock | TagEnd::Table
            )
        {
            break;
        }
        if matches!(tag, TagEnd::List(_))
            && blocks
                .get(index + 1)
                .is_some_and(|(_, next)| !text[next.start..].contains('\n'))
        {
            break;
        }
        let mut end = range.end;
        for line in text[range.end..].split_inclusive('\n') {
            if !line.ends_with('\n') || !line.trim().is_empty() {
                break;
            }
            end += line.len();
        }
        if text[..end]
            .trim_end_matches([' ', '\t', '\r'])
            .ends_with("\n\n")
        {
            stable = end;
        }
    }
    stable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::{Span, Style};
    use crate::ui::theme::Color;

    #[test]
    fn commonmark_summaries_keep_setext_titles_reference_links_and_entities() {
        let rows = render(
            "完成\n====\n\n**结果 *已验证*** &amp; &#x4e2d;\n\n[说明][doc]\n\n[doc]: https://example.com/a(b) \"标题\"",
            80,
        );
        assert_eq!(visible(&rows[0]), "完成");
        assert!(rows[0].spans[0].style.bold && rows[0].spans[0].style.underline);
        assert!(rendered_rows(&rows).contains(&"结果 已验证 & 中".to_string()));
        assert!(span(&rows, "已验证").style.bold && span(&rows, "已验证").style.italic);
        assert!(rendered_rows(&rows).contains(&"说明 (https://example.com/a(b))".to_string()));
        assert!(
            !rendered_rows(&rows)
                .iter()
                .any(|line| line.contains("[doc]:"))
        );
    }

    #[test]
    fn lists_keep_continuation_paragraphs_and_nested_code_in_their_container() {
        let source = "1. first\n   continuation\n\n   second paragraph\n\n   ```rs\n   let x = 1;\n   ```\n\n   - nested\n\n2. second";
        let rows = render(source, 40);
        let text = rendered_rows(&rows);
        assert!(text.contains(&"   continuation".to_string()), "{text:?}");
        assert!(
            text.contains(&"   second paragraph".to_string()),
            "{text:?}"
        );
        assert!(text.contains(&"let x = 1;".to_string()), "{text:?}");
        assert!(text.contains(&"   • nested".to_string()), "{text:?}");
        assert!(text.contains(&"2. second".to_string()), "{text:?}");
    }

    #[test]
    fn streaming_does_not_split_lists_or_lose_forward_reference_definitions() {
        for source in [
            "1. first\n\n   continuation\n\n2. second\n\n",
            "> first\n>\n> second\n\n",
            "[说明][doc]\n\n另一个段落\n\n[doc]: https://example.com\n\n",
        ] {
            assert_eq!(stable_prefix(source), 0, "{source}");
        }
        let source = "Intro\n\n    let x = 1;\n";
        assert_eq!(&source[..stable_prefix(source)], "Intro\n\n");
        assert!(
            render(&source[stable_prefix(source)..], 80)[0]
                .text()
                .starts_with('┌')
        );
    }

    #[test]
    fn raw_html_lines_are_safe_and_narrow_tables_keep_header_value_pairs() {
        let rows = render("<div>\nhello\n</div>\n", 40);
        assert!(rows.iter().all(|row| !row.text().contains('\n')));
        let rows = render(
            "| name | result |\n| --- | --- |\n| 中文项目 | **passed** |",
            12,
        );
        assert_eq!(
            rendered_rows(&rows),
            vec!["name: 中文项目", "result: passed"]
        );
        assert!(
            rows[1]
                .spans
                .iter()
                .any(|span| span.text == "passed" && span.style.bold)
        );
    }

    #[test]
    fn short_code_frames_align_and_narrow_header_only_tables_keep_column_separation() {
        for width in [8, 20, 80] {
            let rows = render("```very-long-language-name\nx\n```", width);
            assert_eq!(rows[0].width(), rows.last().unwrap().width());
            assert!(rows[0].width() <= width);
        }
        let rows = render("| long first | long second |\n| --- | --- |", 12);
        assert_eq!(rendered_rows(&rows), vec!["long first  long second"]);
    }

    #[test]
    fn stable_stream_blocks_hold_open_fences_and_partial_tables() {
        for text in [
            "段落\n\n尾",
            "段落\n\n```rs\nfirst\n\nsecond\n",
            "段落\n\n| A | B |\n| - | - |\n",
        ] {
            assert_eq!(&text[..stable_prefix(text)], "段落\n\n");
        }
        let closed = "段落\n\n~~~~\n\ncode\n~~~~\n\n尾";
        assert_eq!(
            &closed[..stable_prefix(closed)],
            "段落\n\n~~~~\n\ncode\n~~~~\n\n"
        );
        assert_eq!(stable_prefix("尚未结束的段落"), 0);
    }

    #[test]
    fn summaries_render_combined_emphasis_escapes_and_code_delimiters() {
        let rows = render(
            "### 完成 ###\n***已验证***：\\*原文\\*，`` `code` ``，~~旧版~~",
            80,
        );
        assert_eq!(rows[0].text(), "完成");
        assert_eq!(rows[1].text(), "已验证：*原文*，`code`，旧版");
        assert!(span(&rows, "已验证").style.bold);
        assert!(span(&rows, "已验证").style.italic);
        assert!(span(&rows, "旧版").style.crossed_out);
        assert_eq!(rendered("foo__bar__baz"), vec!["foo__bar__baz"]);
    }

    #[test]
    fn summary_links_keep_parentheses_spaces_and_formatted_labels() {
        let rows = render(
            "[**文件**](</tmp/my project/a(b).rs:12>) [说明](https://example.com/a(b))",
            100,
        );
        assert_eq!(
            rows[0].text(),
            "文件 (/tmp/my project/a(b).rs:12) 说明 (https://example.com/a(b))"
        );
        assert!(span(&rows, "文件").style.bold && span(&rows, "文件").style.underline);
    }

    #[test]
    fn tables_measure_rendered_cells_and_keep_literal_pipes() {
        let rows = render("结果 | 值\n--- | ---\n**通过** | `a|b`\n完成 | a\\|b", 120);
        assert!(rows[0].text().starts_with('┌'));
        let widths: Vec<_> = rows.iter().map(Line::width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(rows.iter().any(|r| r.text().contains("a|b")));
    }

    fn rendered(input: &str) -> Vec<String> {
        render(input, 80).iter().map(visible).collect()
    }

    /// The span whose text *is* `needle`. Positional indexing is not used in these tests:
    /// it silently binds a test to how many runs the renderer happens to emit.
    fn span<'a>(rows: &'a [Line], needle: &str) -> &'a Span {
        rows.iter()
            .flat_map(|row| row.spans.iter())
            .find(|span| span.text == needle)
            .unwrap_or_else(|| panic!("no span is {needle:?}: {:?}", rendered_rows(rows)))
    }

    fn rendered_rows(rows: &[Line]) -> Vec<String> {
        rows.iter().map(visible).collect()
    }

    fn visible(line: &Line) -> String {
        line.text()
    }

    #[test]
    fn plain_prose_is_unchanged() {
        assert_eq!(rendered("hello\nworld"), vec!["hello", "world"]);
        assert_eq!(render("hello", 80)[0].spans[0].style, Style::plain());
    }

    #[test]
    fn emphasis_code_and_links_lose_their_marks() {
        let rows = render(
            "see **bold** and `code` plus [docs](https://example.com)",
            80,
        );
        assert_eq!(
            rows[0].text(),
            "see bold and code plus docs (https://example.com)"
        );
        assert!(span(&rows, "bold").style.bold);
        assert_eq!(span(&rows, "code").style.fg, Color::Cyan);
        assert!(span(&rows, "docs").style.underline, "a link is underlined");
        assert_eq!(span(&rows, "docs").style.fg, Color::Cyan);
    }

    #[test]
    fn italics_are_italics_not_grey() {
        // The old renderer painted `*this*` dim, which reads as a hint rather than as
        // emphasis: it is the same colour the program uses for things the eye should skip.
        let rows = render("an *emphasis* here", 80);
        let emphasis = span(&rows, "emphasis");
        assert!(emphasis.style.italic, "{emphasis:?}");
        assert_ne!(
            emphasis.style.fg,
            Color::Dim,
            "emphasis must not be painted as a hint"
        );
    }

    #[test]
    fn an_unclosed_mark_stays_literal() {
        // A stream arrives one token at a time: eating half a `**` would flicker.
        assert_eq!(rendered("still **open"), vec!["still **open"]);
        assert_eq!(rendered("and *this"), vec!["and *this"]);
        assert_eq!(rendered("`half"), vec!["`half"]);
    }

    #[test]
    fn code_marks_are_not_interpreted_inside_a_fence() {
        let rows = render("```rs\nlet x = **no**;\n```", 80);
        // The body is one row, and the `**` is still there: inside a fence nothing is
        // scanned for emphasis.
        assert_eq!(rendered_rows(&rows)[1], "let x = **no**;");
    }

    #[test]
    fn a_fence_becomes_a_labelled_bar_without_its_markers() {
        let text = rendered("before\n```bash\necho hi\n```\nafter");
        // The triple backticks are gone, and a bar has taken their place: leaving them on
        // screen is what made a code block read as a log line.
        assert!(!text.iter().any(|row| row.contains("```")), "{text:?}");
        let open = &text[1];
        assert!(
            open.starts_with("┌─ bash"),
            "the language labels the block: {open:?}"
        );
        assert_eq!(text[2].trim(), "echo hi", "{text:?}");
        assert!(text[3].starts_with('└'), "{text:?}");
    }

    #[test]
    fn a_fence_body_is_passed_through_untouched() {
        // The block is a frame, not a repaint. Whatever a model writes inside a fence —
        // Rust, Kotlin, a language this code has never heard of, or plain prose — reaches
        // the screen character for character, so what a reader copies is what was written.
        for (lang, body) in [
            ("rust", "fn f<'a>(x: &'a str) {}"),
            ("json", "{\"name\": \"ada\", \"ok\": true}"),
            ("klingon", "nuqneH 'ej 'ej"),
        ] {
            let source = format!("```{lang}\n{body}\n```");
            let rows = render(&source, 80);
            assert!(
                rows.iter().any(|row| row.text().contains(body)),
                "the body did not survive a `{lang}` fence intact: {:?}",
                rendered_rows(&rows)
            );
        }
    }

    #[test]
    fn a_fence_without_a_language_still_gets_a_bar() {
        // An unlabelled fence is the common case in a quick answer, and dropping the bar
        // would leave the code indistinguishable from indented prose.
        let text = rendered("```\nplain\n```");
        assert!(text[0].starts_with('┌'), "{text:?}");
        assert_eq!(text[1].trim(), "plain", "{text:?}");
        assert!(text[2].starts_with('└'), "{text:?}");
    }

    #[test]
    fn an_unterminated_fence_keeps_its_frame() {
        // This is the stream tail: the closing fence has not arrived yet. The block has to
        // look like code now, or it changes shape when the last line lands.
        let text = rendered("text\n```py\nprint(1)");
        assert!(text.iter().any(|row| row.contains("print(1)")), "{text:?}");
        assert!(text.iter().all(|row| !row.contains("```")), "{text:?}");
    }

    #[test]
    fn a_table_gets_a_border_and_alignment() {
        let rows = render("| lang | year |\n|:-----|-----:|\n| rust | 2015 |", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].starts_with('┌'), "{text:?}");
        assert!(text[1].contains("lang"), "{text:?}");
        assert!(text[2].starts_with('├'), "{text:?}");
        assert!(text.iter().any(|row| row.contains("rust")), "{text:?}");
        assert!(text.last().unwrap().starts_with('└'), "{text:?}");
        // A right-aligned column pads on the left; the pipe count is the same either way.
        let header = &text[1];
        assert_eq!(header.matches('│').count(), 3, "{header:?}");
        assert!(text[1].contains("│ lang"), "{header:?}");
        assert!(text[1].contains("year │"), "right aligned: {header:?}");
    }

    #[test]
    fn a_streamed_half_table_is_not_a_table_yet() {
        // Without the separator the pipes are just text; eating them would make the answer
        // flicker as the separator row streams in.
        let rows = render("| lang | year |", 80);
        assert_eq!(rendered_rows(&rows), vec!["| lang | year |"]);
    }

    #[test]
    fn a_table_too_wide_for_the_terminal_falls_back_to_text() {
        let wide = "| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb |\n|---|---|\n| 1 | 2 |";
        let rows = render(wide, 30);
        let text = rendered_rows(&rows);
        assert!(!text.iter().any(|row| row.contains('│')), "{text:?}");
    }

    #[test]
    fn nested_lists_keep_their_shape_and_indent_their_wrapping() {
        let rows = render("- top\n  - nested\n    - deeper", 80);
        let text = rendered_rows(&rows);
        assert_eq!(text[0], "• top");
        assert_eq!(text[1], "  • nested");
        assert_eq!(text[2], "    • deeper");
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let rows = render("1. first\n2. second", 80);
        assert_eq!(rendered_rows(&rows), vec!["1. first", "2. second"]);
    }

    #[test]
    fn task_lists_get_boxes() {
        let rows = render("- [ ] todo\n- [x] done", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].contains("☐ todo"), "{text:?}");
        assert!(text[1].contains("☑ done"), "{text:?}");
    }

    #[test]
    fn quotes_nest_by_depth() {
        let rows = render("> outer\n>> inner", 80);
        let text = rendered_rows(&rows);
        assert!(text[0].starts_with("│ outer"), "{text:?}");
        assert!(text[1].starts_with("│ │ inner"), "{text:?}");
    }

    #[test]
    fn headings_are_emphasised() {
        let rows = render("# Title\n## Sub\n### Smaller", 80);
        assert_eq!(rendered_rows(&rows), vec!["Title", "Sub", "Smaller"]);
        assert!(
            rows[0].spans[0].style.bold && rows[0].spans[0].style.underline,
            "h1 is underlined"
        );
        assert!(
            rows[1].spans[0].style.bold && !rows[1].spans[0].style.underline,
            "h2 is bold only"
        );
        assert!(
            rows[2].spans[0].style.bold && rows[2].spans[0].style.italic,
            "h3 steps down"
        );
        assert_eq!(rows[0].spans[0].style.fg, Color::Text);
    }

    #[test]
    fn a_quote_is_not_painted_as_a_hint() {
        let rows = render("> said", 80);
        let said = span(&rows, "said");
        assert_ne!(said.style.fg, Color::Dim);
        assert!(said.style.italic);
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|s| s.text.starts_with('│') && s.style.fg == Color::Dim)
        );
    }

    #[test]
    fn rules_lose_their_syntax() {
        for input in ["---", "***", "___", "- - -"] {
            let rows = render(input, 80);
            assert_eq!(rendered_rows(&rows), vec!["─".repeat(24)], "{input}");
        }
        // But a rule inside a fenced block is code.
        let rows = render("```\n---\n```", 80);
        assert_eq!(rendered_rows(&rows)[1], "---");
    }

    #[test]
    fn blank_line_runs_collapse() {
        assert_eq!(rendered("a\n\n\n\nb"), vec!["a", "", "b"]);
        // And none are left dangling at the end.
        assert_eq!(rendered("a\n\n\n"), vec!["a"]);
    }

    #[test]
    fn underscores_inside_a_name_are_not_italic() {
        assert_eq!(rendered("foo_bar_baz"), vec!["foo_bar_baz"]);
    }

    #[test]
    fn strikethrough_keeps_its_text() {
        let rows = render("~~gone~~ kept", 80);
        assert_eq!(rows[0].text(), "gone kept");
        assert!(span(&rows, "gone").style.crossed_out);
    }

    #[test]
    fn a_bare_url_is_underlined_without_its_trailing_stop() {
        let rows = render("see https://example.com/a_b.", 80);
        let line = &rows[0];
        let url = line
            .spans
            .iter()
            .find(|span| span.text.starts_with("https://"))
            .expect("the url is its own span");
        assert!(url.style.underline);
        assert_eq!(url.text, "https://example.com/a_b");
        // The sentence's full stop is left in place.
        assert_eq!(rows[0].text(), "see https://example.com/a_b.");
    }

    #[test]
    fn an_image_shows_its_alt_text() {
        let rows = render("![a chart](https://x/y.png)", 80);
        let text = rows[0].text();
        assert!(text.contains("a chart"), "{text}");
        assert!(
            !text.contains("https://x/y.png"),
            "the url is not shown: {text}"
        );
    }

    #[test]
    fn nothing_interprets_ansii_from_the_model() {
        let rows = render("\u{1b}[31mred\u{1b}[0m", 80);
        assert!(rows.iter().all(|row| !row.text().contains('\u{1b}')));
    }

    /// Render every prefix of `text` the way a stream would, and hand back each frame.
    fn frames(text: &str) -> Vec<Vec<String>> {
        (1..=text.len())
            .filter(|end| text.is_char_boundary(*end))
            .map(|end| {
                let lines = crate::ui::text::history_rows(&render(&text[..end], 60), 60);
                lines.iter().map(|row| row.line.text()).collect()
            })
            .collect()
    }

    #[test]
    fn a_table_that_has_appeared_never_changes_shape() {
        // The separator is what turns these rows into a table, and it arrives one character
        // at a time. The grid must not appear, vanish and reappear as the dashes land: that
        // is a whole block flashing under the reader on every keystroke of the model's
        // output. So the assertion is monotonicity — once a frame has a grid, every later
        // frame has the same one, growing only at the bottom.
        let mut seen: Option<Vec<String>> = None;
        for frame in frames("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n") {
            let grid: Vec<String> = frame
                .iter()
                .filter(|row| row.starts_with('│'))
                .cloned()
                .collect();
            if grid.is_empty() {
                // Not a table yet: the pipes are still text, which is readable either way.
                assert!(
                    !frame.iter().any(|row| row.starts_with('┌')),
                    "border without cells: {frame:?}"
                );
                continue;
            }
            match &seen {
                None => {
                    // The first grid must be the widest one — the header row, with its two
                    // columns — or the columns would re-space as more rows arrive.
                    assert_eq!(grid[0].matches('│').count(), 3, "{frame:?}");
                }
                Some(previous) => {
                    assert!(
                        grid.len() >= previous.len(),
                        "the table shrank: {previous:?} -> {grid:?}"
                    );
                    // Rows already drawn keep their column layout. A cell's *text* fills in
                    // as its characters arrive — that is the point of streaming — but the
                    // pipes must not move, because that is the grid re-spacing itself.
                    for (was, now) in previous.iter().zip(&grid) {
                        let bars = |row: &String| {
                            row.match_indices('│').map(|(at, _)| at).collect::<Vec<_>>()
                        };
                        assert_eq!(bars(was), bars(now), "columns moved: {was:?} -> {now:?}");
                    }
                }
            }
            seen = Some(grid);
        }
        assert!(seen.is_some(), "the table never appeared");
    }

    #[test]
    fn a_streamed_fence_never_shows_its_backticks() {
        // The opening fence turns into a bar immediately. Leaving the backticks visible and
        // swapping them for a bar at the end would be the block changing shape under the
        // reader's eyes.
        for frame in frames("```rust\nfn f() {}\n```") {
            assert!(
                !frame.iter().any(|row| row.contains("```")),
                "a fence marker reached the screen: {frame:?}"
            );
        }
    }

    #[test]
    fn a_bullet_is_never_left_alone_on_its_row() {
        // A plain space after the marker is a break opportunity, so at a narrow width the
        // bullet ends up on a row of its own with the text under it — which reads as a lost
        // item rather than a list. The marker is glued to the first word instead.
        let source = "- a list item long enough to need wrapping at a narrow width";
        for width in [28usize, 20, 16] {
            let lines = crate::ui::text::history_rows(&render(source, width), width);
            assert!(
                lines[0].line.text().trim_end().len() > 1,
                "width {width}: the bullet is alone: {:?}",
                lines.iter().map(|row| row.line.text()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn nothing_wraps_past_its_width_once_the_screen_has_wrapped_it() {
        let source = "- a list item long enough to wrap\n\n> a quote that also wraps around\n\n| a | b |\n|---|---|\n| one | two |";
        for width in [20usize, 40, 80] {
            for line in crate::ui::text::history_rows(&render(source, width), width) {
                assert!(
                    util::width(&line.line.text()) <= width,
                    "{width}: {:?}",
                    line.line.text()
                );
            }
        }
    }
}
