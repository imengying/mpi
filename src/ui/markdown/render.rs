//! CommonMark events become the same styled lines used by the rest of the TUI.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Tag, TagEnd};

use super::{code, parser};
use crate::ui::text::{Line, Span, Style};
use crate::ui::theme::Color;
use crate::util;

enum Container {
    Quote,
    Item { marker: String, first: bool },
}

pub(super) fn render(text: &str, width: usize) -> Vec<Line> {
    let mut writer = Writer {
        source: text,
        width,
        lines: Vec::new(),
        current: Vec::new(),
        styles: vec![Style::plain()],
        containers: Vec::new(),
        lists: Vec::new(),
        links: Vec::new(),
        code: None,
        table: None,
    };
    for (event, range) in parser(text).into_offset_iter() {
        writer.event(event, range);
    }
    writer.flush();
    while writer
        .lines
        .last()
        .is_some_and(|line| line.text().trim().is_empty())
    {
        writer.lines.pop();
    }
    writer.lines
}

struct Writer<'a> {
    source: &'a str,
    width: usize,
    lines: Vec<Line>,
    current: Vec<Span>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    lists: Vec<Option<u64>>,
    links: Vec<(String, String)>,
    code: Option<(String, String)>,
    table: Option<code::Table>,
}

impl Writer<'_> {
    fn style(&self) -> Style {
        *self.styles.last().unwrap()
    }

    fn append(&mut self, text: &str, style: Style) {
        let spans = match &mut self.table {
            Some(table) => &mut table.cell,
            None => &mut self.current,
        };
        if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
            last.text.push_str(text);
        } else if !text.is_empty() {
            spans.push(Span::new(text, style));
        }
    }

    fn text(&mut self, text: &str) {
        if let Some((_, label)) = self.links.last_mut() {
            label.push_str(text);
        }
        let style = self.style();
        // CommonMark does not autolink bare URLs. Give those the same visible underline
        // without parsing Markdown punctuation a second time.
        let mut rest = text;
        while let Some(at) = [rest.find("https://"), rest.find("http://")]
            .into_iter()
            .flatten()
            .min()
        {
            self.append(&rest[..at], style);
            rest = &rest[at..];
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', ';', '!', '?', ')', ']', '"', '\'']);
            if url.is_empty() {
                break;
            }
            self.append(
                url,
                Style {
                    underline: true,
                    ..style
                },
            );
            rest = &rest[url.len()..];
        }
        self.append(rest, style);
    }

    fn prefix(&mut self) -> Vec<Span> {
        self.containers
            .iter_mut()
            .map(|container| match container {
                Container::Quote => Span::new("│ ", Style::new(Color::Dim)),
                Container::Item { marker, first } => {
                    let text = if *first {
                        marker.clone()
                    } else {
                        " ".repeat(util::width(marker))
                    };
                    *first = false;
                    Span::new(text, Style::new(Color::Dim))
                }
            })
            .collect()
    }

    fn flush(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let mut spans = self.prefix();
        spans.append(&mut self.current);
        self.lines.push(Line::spans(spans));
    }

    fn start_block(&mut self, at: usize) {
        self.flush();
        let line_start = self.source[..at].rfind('\n').map_or(0, |at| at + 1);
        let previous = self.source[..line_start]
            .strip_suffix('\n')
            .and_then(|text| text.rsplit('\n').next());
        if previous.is_some_and(|line| line.trim().is_empty())
            && self
                .lines
                .last()
                .is_some_and(|line| !line.text().trim().is_empty())
        {
            self.lines.push(Line::blank());
        }
    }

    fn extend(&mut self, lines: Vec<Line>) {
        for line in lines {
            let mut prefix = self.prefix();
            prefix.extend(line.spans);
            self.lines.push(Line::spans(prefix));
        }
    }

    fn event(&mut self, event: Event<'_>, range: std::ops::Range<usize>) {
        if let Some((_, body)) = &mut self.code {
            match event {
                Event::Text(text) => body.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let (language, body) = self.code.take().unwrap();
                    let width = self.width.saturating_sub(self.container_width());
                    let mut frame = code::frame(&language, &body, width).into_iter();
                    let top = frame.next().unwrap();
                    let bottom = frame.next_back().unwrap();
                    // Container prefixes belong on the borders; code keeps its own indent
                    // so ordinary terminal selection produces usable code in lists/quotes.
                    self.extend(vec![top]);
                    self.lines.extend(frame);
                    self.extend(vec![bottom]);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => self.start_block(range.start),
                Tag::Heading { level, .. } => {
                    self.start_block(range.start);
                    self.styles.push(heading_style(level));
                }
                Tag::BlockQuote(_) => {
                    self.start_block(range.start);
                    self.containers.push(Container::Quote);
                    self.styles.push(Style {
                        italic: true,
                        ..self.style()
                    });
                }
                Tag::List(start) => {
                    self.start_block(range.start);
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.start_block(range.start);
                    let marker = match self.lists.last_mut().and_then(Option::as_mut) {
                        Some(number) => {
                            let marker = format!("{number}. ");
                            *number = number.saturating_add(1);
                            marker
                        }
                        None => "• ".to_string(),
                    };
                    self.containers.push(Container::Item {
                        marker,
                        first: true,
                    });
                }
                Tag::CodeBlock(kind) => {
                    self.start_block(range.start);
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    self.code = Some((language, String::new()));
                }
                Tag::Table(alignments) => {
                    self.start_block(range.start);
                    self.table = Some(code::Table::new(alignments));
                }
                Tag::TableHead | Tag::TableRow => {}
                Tag::TableCell => {
                    if let Some(table) = &self.table {
                        self.styles.push(Style {
                            bold: table.rows.is_empty(),
                            ..self.style()
                        });
                    }
                }
                Tag::Emphasis => self.styles.push(Style {
                    italic: true,
                    ..self.style()
                }),
                Tag::Strong => self.styles.push(Style {
                    bold: true,
                    ..self.style()
                }),
                Tag::Strikethrough => self.styles.push(Style {
                    crossed_out: true,
                    ..self.style()
                }),
                Tag::Link { dest_url, .. } => {
                    self.links.push((dest_url.to_string(), String::new()));
                    self.styles.push(Style {
                        fg: Color::Cyan,
                        underline: true,
                        ..self.style()
                    });
                }
                Tag::Image { .. } => {
                    self.append("🖼 ", Style::new(Color::Dim));
                    self.styles.push(Style::new(Color::Dim));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => self.flush(),
                TagEnd::Heading(_) => {
                    self.flush();
                    self.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    self.containers.pop();
                    self.styles.pop();
                }
                TagEnd::Item => {
                    self.flush();
                    self.containers.pop();
                }
                TagEnd::List(_) => {
                    self.flush();
                    self.lists.pop();
                }
                TagEnd::TableCell => {
                    if let Some(table) = &mut self.table {
                        table.row.push(std::mem::take(&mut table.cell));
                    }
                    self.styles.pop();
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = &mut self.table {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                }
                TagEnd::Table => {
                    let table = self.table.take().unwrap();
                    let width = self.width.saturating_sub(self.container_width());
                    self.extend(table.render(width));
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                    self.styles.pop();
                }
                TagEnd::Link => {
                    self.styles.pop();
                    if let Some((destination, label)) = self.links.pop()
                        && !destination.is_empty()
                        && destination != label
                    {
                        self.append(" ", Style::plain());
                        self.append(&format!("({destination})"), Style::new(Color::Dim));
                    }
                }
                _ => {}
            },
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                if let Some((_, label)) = self.links.last_mut() {
                    label.push_str(&text);
                }
                self.append(
                    &text,
                    Style {
                        fg: Color::Cyan,
                        ..self.style()
                    },
                );
            }
            // Keep explicit model line breaks, including list/quote continuation prefixes.
            Event::SoftBreak | Event::HardBreak => {
                if self.table.is_some() {
                    self.append(" ", self.style());
                } else {
                    self.flush();
                }
            }
            Event::Rule => {
                self.start_block(range.start);
                self.extend(vec![Line::dim("─".repeat(self.width.clamp(1, 24)))]);
            }
            Event::TaskListMarker(checked) => {
                if let Some(Container::Item { marker, .. }) = self.containers.last_mut() {
                    *marker = format!("{} ", if checked { '☑' } else { '☐' });
                }
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                for (index, line) in text.split('\n').enumerate() {
                    if index > 0 {
                        self.flush();
                    }
                    self.append(line, self.style());
                }
            }
            _ => {}
        }
    }

    fn container_width(&self) -> usize {
        self.containers
            .iter()
            .map(|container| match container {
                Container::Quote => 2,
                Container::Item { marker, .. } => util::width(marker),
            })
            .sum()
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style {
            bold: true,
            underline: true,
            ..Style::plain()
        },
        HeadingLevel::H2 => Style::bold(Color::Text),
        HeadingLevel::H3 => Style {
            bold: true,
            italic: true,
            ..Style::plain()
        },
        _ => Style::italic(Color::Text),
    }
}
