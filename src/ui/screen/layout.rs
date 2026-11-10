//! Cache visible streaming text and a bounded set of immutable history blocks.
use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One append-only answer. Input and spinner changes reuse its visible tail.
#[derive(Default)]
pub(super) struct PreviewCache(RefCell<Option<Preview>>);

struct Preview {
    bytes: usize,
    width: usize,
    height: usize,
    rendered: Instant,
    rows: Arc<Vec<HistoryRow<Line>>>,
}

impl PreviewCache {
    pub(super) fn clear(&mut self) {
        *self.0.get_mut() = None;
    }

    pub(super) fn tail(
        &self,
        text: &str,
        width: usize,
        height: usize,
    ) -> Arc<Vec<HistoryRow<Line>>> {
        self.tail_at(text, width, height, Instant::now())
    }

    fn tail_at(
        &self,
        text: &str,
        width: usize,
        height: usize,
        now: Instant,
    ) -> Arc<Vec<HistoryRow<Line>>> {
        let mut cached = self.0.borrow_mut();
        if let Some(entry) = cached.as_ref()
            && entry.width == width
            && entry.height == height
            && (entry.bytes == text.len()
                // Long replies need at most 20 layouts/s. Input still draws at 60 fps;
                // the working ticker flushes the last chunk, and completion renders in full.
                || (text.len() >= 16 * 1024
                    && now.duration_since(entry.rendered) < Duration::from_millis(50)))
        {
            return entry.rows.clone();
        }
        // Parse the whole source so reference links, lists and unfinished code fences
        // keep their context. Retain only rows that can be visible, not the whole reply.
        let mut rows =
            crate::ui::text::history_rows(&crate::ui::markdown::render(text, width), width);
        let rows = Arc::new(rows.split_off(rows.len().saturating_sub(height)));
        *cached = Some(Preview {
            bytes: text.len(),
            width,
            height,
            rendered: now,
            rows: rows.clone(),
        });
        rows
    }
}

#[derive(Default)]
pub(super) struct LayoutCache {
    entries: VecDeque<Entry>,
    bytes: usize,
}
struct Entry {
    index: usize,
    width: usize,
    rows: Arc<Vec<HistoryRow<Line>>>,
    bytes: usize,
}
const MAX_ENTRIES: usize = 64;
const MAX_BYTES: usize = 8 * 1024 * 1024;

impl LayoutCache {
    pub(super) fn rows(
        &mut self,
        index: usize,
        width: usize,
        block: &Block,
    ) -> Arc<Vec<HistoryRow<Line>>> {
        if let Some(position) = self
            .entries
            .iter()
            .position(|entry| entry.index == index && entry.width == width)
        {
            let entry = self.entries.remove(position).expect("cached entry");
            let rows = entry.rows.clone();
            self.entries.push_back(entry);
            return rows;
        }
        let rows: Arc<Vec<_>> = Arc::new(block.history_rows(width));
        let bytes = rows
            .iter()
            .map(|row| {
                std::mem::size_of_val(row)
                    + row
                        .line
                        .spans
                        .iter()
                        .map(|span| std::mem::size_of_val(span) + span.text.len())
                        .sum::<usize>()
            })
            .sum();
        if bytes <= MAX_BYTES {
            while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > MAX_BYTES {
                if let Some(entry) = self.entries.pop_front() {
                    self.bytes -= entry.bytes;
                }
            }
            self.bytes += bytes;
            self.entries.push_back(Entry {
                index,
                width,
                rows: rows.clone(),
                bytes,
            });
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_layout_reuses_unchanged_text_and_refreshes_the_visible_tail() {
        let mut cache = PreviewCache::default();
        let now = Instant::now();
        let mut source = "- long list item with **emphasis**\n".repeat(1000);
        let first = cache.tail_at(&source, 40, 24, now);
        assert_eq!(first.len(), 24);
        assert!(Arc::ptr_eq(
            &first,
            &cache.tail_at(&source, 40, 24, now + Duration::from_millis(1))
        ));
        source.push_str("\nNEW_TAIL");
        assert!(Arc::ptr_eq(
            &first,
            &cache.tail_at(&source, 40, 24, now + Duration::from_millis(10))
        ));
        let fresh = cache.tail_at(&source, 40, 24, now + Duration::from_millis(50));
        assert_eq!(fresh.last().unwrap().line.text(), "NEW_TAIL");
        for (width, height) in [(12, 8), (80, 32)] {
            let resized = cache.tail_at(&source, width, height, now + Duration::from_millis(51));
            let expected =
                crate::ui::text::history_rows(&crate::ui::markdown::render(&source, width), width);
            assert_eq!(resized.as_slice(), &expected[expected.len() - height..]);
        }
        let next = format!(
            "{}\nNEXT_REPLY",
            "x".repeat(source.len() - "\nNEXT_REPLY".len())
        );
        cache.clear();
        assert_eq!(
            cache
                .tail_at(&next, 80, 32, now + Duration::from_millis(52))
                .last()
                .unwrap()
                .line
                .text(),
            "NEXT_REPLY"
        );
    }
    #[test]
    fn caches_widths_and_evicts_old_entries() {
        let mut cache = LayoutCache::default();
        let block = Block::markdown("hello world");
        let first = cache.rows(0, 40, &block);
        assert!(Arc::ptr_eq(&first, &cache.rows(0, 40, &block)));
        assert!(!Arc::ptr_eq(&first, &cache.rows(0, 20, &block)));
        for index in 1..1000 {
            cache.rows(index, 40, &block);
        }
        assert_eq!(cache.entries.len(), MAX_ENTRIES);
        assert!(cache.bytes <= MAX_BYTES);
        assert!(!Arc::ptr_eq(&first, &cache.rows(0, 40, &block)));
        let mut screen = Screen::new();
        screen.out = Box::new(Vec::<u8>::new());
        screen.blocks = (0..10_000)
            .map(|n| Block::markdown(format!("entry {n}")))
            .collect();
        screen.printed = screen.blocks.len();
        screen.editing = Some(Editor::new());
        screen.live_frame();
        for width in [30, 80, 12] {
            screen.width = width;
            screen.live_frame();
            assert!(screen.layout.entries.len() <= MAX_ENTRIES);
            assert!(
                screen
                    .layout
                    .entries
                    .iter()
                    .all(|entry| entry.index >= 9_970)
            );
        }
        screen.transcript_event(&Event::Mouse(event::MouseEvent {
            kind: event::MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        screen.transcript_event(&Event::Key(KeyCode::Home.into()));
        screen.draw_transcript();
        assert!(screen.layout.entries.iter().any(|entry| entry.index == 0));
        assert!(
            screen
                .layout
                .entries
                .iter()
                .all(|entry| entry.index < 30 || entry.index >= 9_970)
        );
    }
}
