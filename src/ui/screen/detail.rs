//! Temporary history browser. It never mutates transcript blocks or the composer.
use super::*;
use selection::{Point, Selection};

pub(super) struct Detail {
    index: usize,
    offset: usize,
    rows: Vec<HistoryRow<Line>>,
    width: usize,
    query: String,
    searching: bool,
    found: Option<usize>,
    live_log: Option<std::path::PathBuf>,
    refreshed: Option<std::time::Instant>,
    follow: bool,
    selection: Option<Selection>,
}

impl Detail {
    pub(super) fn has_selection(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    pub(super) fn is_live(&self) -> bool {
        self.live_log.is_some()
    }

    fn refresh(&mut self, blocks: &[Block], width: usize) {
        if self.width != width {
            self.selection = None;
        } else if self.selection.is_some() {
            return;
        }
        if let Some(path) = &self.live_log {
            if self.width != width
                || self
                    .refreshed
                    .is_none_or(|time| time.elapsed().as_millis() >= 250)
            {
                let text =
                    read_log_tail(path).unwrap_or_else(|err| format!("无法读取运行日志：{err}"));
                self.rows = crate::ui::text::history_rows(
                    &text
                        .lines()
                        .map(|line| Line::plain(util::sanitize(line)))
                        .collect::<Vec<_>>(),
                    width,
                );
                self.width = width;
                self.refreshed = Some(std::time::Instant::now());
            }
            return;
        }
        if self.width != width {
            let mut block = blocks[self.index].clone();
            if let Block::Collapsible(inner) = &mut block {
                inner.expanded = true;
                if let Some(path) = &inner.full_output {
                    match read_log(path) {
                        Ok(text) => {
                            inner.lines.truncate(inner.head);
                            let text = crate::ui::compact::result_text(
                                crate::llm::ToolStatus::Success,
                                &text,
                            );
                            inner
                                .lines
                                .extend(text.lines().map(|line| Line::plain(util::sanitize(line))));
                        }
                        Err(err) => inner
                            .lines
                            .push(Line::dim(format!("无法读取完整日志：{err}"))),
                    }
                }
            }
            self.rows = block.history_rows(width);
            self.width = width;
        }
    }
}

impl Screen {
    pub(super) fn open_detail(&mut self) -> bool {
        if self.detail.is_some() {
            self.close_detail();
            return true;
        }
        let live_log = self.running_log.clone();
        let index = if live_log.is_some() {
            Some(self.blocks.len())
        } else {
            self.blocks.iter().rposition(Block::is_collapsible)
        };
        let Some(index) = index else {
            self.notice = Some("没有工具详情".into());
            self.render();
            return false;
        };
        if !self.interactive {
            return false;
        }
        self.close_transcript();
        self.draw_live();
        let _ = crossterm::execute!(self.out, cursor::Hide);
        self.detail = Some(Detail {
            index,
            offset: 0,
            rows: Vec::new(),
            width: 0,
            query: String::new(),
            searching: false,
            found: None,
            follow: live_log.is_some(),
            selection: None,
            live_log,
            refreshed: None,
        });
        self.draw_detail();
        true
    }

    pub(super) fn close_detail(&mut self) {
        self.copy_menu = None;
        if self.detail.take().is_none() {
            return;
        }
        self.erase_live();
        self.draw_live();
    }

    pub(super) fn detail_event(&mut self, event: Event) {
        if let Event::Resize(width, height) = event {
            self.width = usize::from(width.max(1));
            self.height = usize::from(height.max(1));
            return;
        }
        if let Event::Mouse(mouse) = event {
            self.detail_mouse(mouse);
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.detail.as_ref().is_some_and(Detail::has_selection)
        {
            self.copy_selection();
            return;
        }
        let detail = self.detail.as_mut().expect("open detail");
        detail.selection = None;
        detail.refresh(&self.blocks, self.width);
        if detail.searching {
            match key.code {
                KeyCode::Esc => detail.searching = false,
                KeyCode::Enter => {
                    detail.searching = false;
                    find_next(detail, true);
                }
                KeyCode::Backspace => {
                    detail.query.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    detail.query.push(c)
                }
                _ => {}
            }
            return;
        }
        let page = self.height.saturating_sub(2).max(1);
        if matches!(
            key.code,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Home | KeyCode::Char('/')
        ) {
            detail.follow = false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_detail(),
            KeyCode::Char('o' | 'c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_detail()
            }
            KeyCode::Up | KeyCode::Char('k') => detail.offset = detail.offset.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => detail.offset = detail.offset.saturating_add(1),
            KeyCode::Char(' ') => detail.offset = detail.offset.saturating_add(page),
            KeyCode::Home => detail.offset = 0,
            KeyCode::End => {
                detail.offset = usize::MAX;
                detail.follow = detail.is_live();
            }
            KeyCode::Left | KeyCode::Char('[') if !detail.is_live() => {
                if let Some(index) = self.blocks[..detail.index]
                    .iter()
                    .rposition(Block::is_collapsible)
                {
                    detail.index = index;
                    detail.width = 0;
                    detail.found = None;
                    detail.offset = 0;
                }
            }
            KeyCode::Right | KeyCode::Char(']') if !detail.is_live() => {
                if let Some(index) = self.blocks[detail.index + 1..]
                    .iter()
                    .position(Block::is_collapsible)
                {
                    detail.index += index + 1;
                    detail.width = 0;
                    detail.found = None;
                    detail.offset = 0;
                }
            }
            KeyCode::Char('/') => {
                detail.query.clear();
                detail.searching = true;
            }
            KeyCode::Char('n') => find_next(detail, false),
            _ => {}
        }
    }

    fn detail_mouse(&mut self, mouse: event::MouseEvent) {
        use event::{MouseButton::Left, MouseEventKind::*};
        let detail = self.detail.as_mut().expect("open detail");
        detail.refresh(&self.blocks, self.width);
        let page = self.height.saturating_sub(2).max(1);
        let row = usize::from(mouse.row).saturating_sub(1).min(page - 1);
        let point = Point {
            block: 0,
            row: (detail.offset + row).min(detail.rows.len().saturating_sub(1)),
            column: usize::from(mouse.column),
        };
        match mouse.kind {
            ScrollUp | ScrollDown => {
                detail.follow = false;
                detail.offset = if mouse.kind == ScrollUp {
                    detail.offset.saturating_sub(3)
                } else {
                    (detail.offset + 3).min(detail.rows.len().saturating_sub(page))
                };
                if let Some(selection) = detail.selection.as_mut()
                    && let Some((row, column)) = selection.dragging_pointer()
                {
                    selection.extend_to(Point {
                        block: 0,
                        row: (detail.offset + row.saturating_sub(1).min(page - 1))
                            .min(detail.rows.len().saturating_sub(1)),
                        column,
                    });
                }
            }
            Down(Left)
                if usize::from(mouse.row) > 0
                    && usize::from(mouse.row) < self.height.saturating_sub(1)
                    && !detail.rows.is_empty() =>
            {
                detail.follow = false;
                detail.selection = Some(Selection::new(point, mouse));
            }
            Drag(Left) | Up(Left) => {
                if let Some(selection) = detail
                    .selection
                    .as_mut()
                    .filter(|selection| selection.dragging)
                {
                    selection.update(point, mouse);
                }
            }
            _ => {}
        }
    }

    pub(super) fn tick_detail_selection(&mut self, now: std::time::Instant) -> bool {
        let Some(detail) = self.detail.as_mut() else {
            return false;
        };
        let Some(selection) = detail.selection.as_mut() else {
            return false;
        };
        let Some((delta, row, column)) = selection.scroll(now, 1, self.height.saturating_sub(2))
        else {
            return false;
        };
        let page = self.height.saturating_sub(2).max(1);
        detail.offset = if delta > 0 {
            detail.offset.saturating_sub(delta as usize)
        } else {
            (detail.offset + delta.unsigned_abs()).min(detail.rows.len().saturating_sub(page))
        };
        selection.extend_to(Point {
            block: 0,
            row: (detail.offset + row.saturating_sub(1).min(page - 1))
                .min(detail.rows.len().saturating_sub(1)),
            column,
        });
        true
    }

    pub(super) fn detail_selection(&self) -> String {
        let mut text = String::new();
        if let Some(detail) = &self.detail
            && let Some(selection) = &detail.selection
        {
            for (row, line) in detail.rows.iter().enumerate() {
                selection.append(
                    &mut text,
                    line,
                    Point {
                        block: 0,
                        row,
                        column: 0,
                    },
                );
            }
        }
        text
    }

    pub(super) fn draw_detail(&mut self) {
        let Some(mut detail) = self.detail.take() else {
            return;
        };
        detail.refresh(&self.blocks, self.width);
        let page = self.height.saturating_sub(2);
        if detail.follow {
            detail.offset = usize::MAX;
        }
        detail.offset = detail
            .offset
            .min(detail.rows.len().saturating_sub(page.max(1)));
        let title = if detail.is_live() {
            format!(
                "运行日志 · {}s · End 跟随 · Esc 返回",
                self.running_started
                    .map_or(0, |time| time.elapsed().as_secs())
            )
        } else {
            format!(
                "工具详情 · {} · ←/→ 切换 · / 搜索 · Esc 返回",
                detail.index + 1
            )
        };
        let footer = if detail.searching {
            format!("/{}", detail.query)
        } else {
            format!(
                "{}/{} · 滚轮浏览 · 右键复制 · n 下一个",
                detail.offset + 1,
                detail.rows.len()
            )
        };
        let mut frame = String::from("\x1b[?2026h\x1b[H\x1b[2J\x1b[?25l");
        let mut lines = vec![Line::dim(util::truncate(&title, self.width, "…"))];
        lines.extend(
            detail
                .rows
                .iter()
                .enumerate()
                .skip(detail.offset)
                .take(page)
                .map(|(row, line)| {
                    detail.selection.as_ref().map_or_else(
                        || line.line.clone(),
                        |selection| {
                            selection.paint(
                                &line.line,
                                Point {
                                    block: 0,
                                    row,
                                    column: 0,
                                },
                            )
                        },
                    )
                }),
        );
        if self.height > 1 {
            while lines.len() < self.height - 1 {
                lines.push(Line::blank());
            }
            lines.push(Line::dim(util::truncate(&footer, self.width, "…")));
        }
        for (row, line) in lines.iter().enumerate() {
            frame.push_str(&format!("\x1b[{};1H{}", row + 1, self.paint(line)));
        }
        frame.push_str("\x1b[?2026l");
        let _ = self.out.write_all(frame.as_bytes());
        let _ = self.out.flush();
        self.detail = Some(detail);
    }
}

fn read_log_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let offset = file.metadata()?.len().saturating_sub(64 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    let start = if offset > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .filter(|i| *i < bytes.len())
            .unwrap_or_else(|| bytes.iter().take_while(|b| **b & 0xc0 == 0x80).count())
    } else {
        0
    };
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn find_next(detail: &mut Detail, from_start: bool) {
    if detail.query.is_empty() || detail.rows.is_empty() {
        return;
    }
    let start = if from_start {
        0
    } else {
        (detail.found.unwrap_or(detail.offset) + 1) % detail.rows.len()
    };
    let needle = detail.query.to_lowercase();
    if let Some(index) = (start..detail.rows.len()).chain(0..start).find(|index| {
        detail.rows[*index]
            .line
            .text()
            .to_lowercase()
            .contains(&needle)
    }) {
        detail.offset = index;
        detail.found = Some(index);
    }
}

fn read_log(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    // Bound the browser's allocation independently of the model's much smaller output budget.
    const LIMIT: u64 = 8 * 1024 * 1024;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    let more = bytes.len() as u64 > LIMIT;
    bytes.truncate(LIMIT as usize);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if more {
        text.push_str(&format!("\n详情显示前 8 MiB；完整日志：{}", path.display()));
    }
    Ok(text)
}
