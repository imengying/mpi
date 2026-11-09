//! Bounded cache of immutable history blocks, indexed by block and terminal width.
use super::*;
use std::collections::VecDeque;
use std::sync::Arc;

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
        screen.transcript_event(&Event::Key(KeyCode::PageUp.into()));
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
