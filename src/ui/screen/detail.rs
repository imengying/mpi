//! Temporary history browser. It never mutates transcript blocks or the composer.
use super::*;

pub(super) struct Detail {
    index: usize,
    offset: usize,
    rows: Vec<Line>,
    width: usize,
    query: String,
    searching: bool,
    found: Option<usize>,
}

impl Detail {
    fn refresh(&mut self, blocks: &[Block], width: usize) {
        if self.width != width {
            let mut block = blocks[self.index].clone();
            if let Block::Collapsible(inner) = &mut block {
                inner.expanded = true;
                if let Some(path) = &inner.full_output {
                    match read_log(path) {
                        Ok(text) => {
                            inner.lines.truncate(inner.head);
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
            self.rows = block
                .history_rows(width)
                .into_iter()
                .map(|row| row.line)
                .collect();
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
        let Some(index) = self.blocks.iter().rposition(Block::is_collapsible) else {
            self.notice = Some("没有工具详情".into());
            self.render();
            return false;
        };
        if !self.interactive {
            return false;
        }
        self.draw_live();
        let _ = crossterm::execute!(self.out, terminal::EnterAlternateScreen, cursor::Hide);
        self.detail = Some(Detail {
            index,
            offset: 0,
            rows: Vec::new(),
            width: 0,
            query: String::new(),
            searching: false,
            found: None,
        });
        self.draw_detail();
        true
    }

    pub(super) fn close_detail(&mut self) {
        if self.detail.take().is_none() {
            return;
        }
        let _ = crossterm::execute!(self.out, terminal::LeaveAlternateScreen, cursor::Show);
        self.draw_live();
    }

    pub(super) fn detail_event(&mut self, event: Event) {
        if let Event::Resize(width, height) = event {
            self.width = usize::from(width.max(1));
            self.height = usize::from(height.max(1));
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        let detail = self.detail.as_mut().expect("open detail");
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
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_detail(),
            KeyCode::Char('o' | 'c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.close_detail()
            }
            KeyCode::Up | KeyCode::Char('k') => detail.offset = detail.offset.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => detail.offset = detail.offset.saturating_add(1),
            KeyCode::PageUp => detail.offset = detail.offset.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                detail.offset = detail.offset.saturating_add(page)
            }
            KeyCode::Home => detail.offset = 0,
            KeyCode::End => detail.offset = usize::MAX,
            KeyCode::Left | KeyCode::Char('[') => {
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
            KeyCode::Right | KeyCode::Char(']') => {
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

    pub(super) fn draw_detail(&mut self) {
        let Some(mut detail) = self.detail.take() else {
            return;
        };
        detail.refresh(&self.blocks, self.width);
        let page = self.height.saturating_sub(2);
        detail.offset = detail
            .offset
            .min(detail.rows.len().saturating_sub(page.max(1)));
        let title = format!(
            "工具详情 · {} · ←/→ 切换 · / 搜索 · Esc 返回",
            detail.index + 1
        );
        let footer = if detail.searching {
            format!("/{}", detail.query)
        } else {
            format!(
                "{}/{} · ↑/↓ 滚动 · PgUp/PgDn 翻页 · n 下一个",
                detail.offset + 1,
                detail.rows.len()
            )
        };
        let mut frame = String::from("\x1b[?2026h\x1b[H\x1b[2J\x1b[?25l");
        let mut lines = vec![Line::dim(util::truncate(&title, self.width, "…"))];
        lines.extend(detail.rows.iter().skip(detail.offset).take(page).cloned());
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
    if let Some(index) = (start..detail.rows.len())
        .chain(0..start)
        .find(|index| detail.rows[*index].text().to_lowercase().contains(&needle))
    {
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
