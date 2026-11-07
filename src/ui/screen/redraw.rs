//! Coalesce requests from typing, streaming, and the spinner.
use std::time::{Duration, Instant};

pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(16);
#[derive(Default)]
pub(super) struct Redraw {
    last: Option<Instant>,
    pending: bool,
}
impl Redraw {
    pub(super) fn request(&mut self) {
        self.pending = true;
    }
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.pending
            && self
                .last
                .is_none_or(|last| now.duration_since(last) >= FRAME_INTERVAL)
    }
    pub(super) fn drawn(&mut self, now: Instant) {
        self.last = Some(now);
        self.pending = false;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_burst_has_one_frame_and_keeps_the_last_request_pending() {
        let mut redraw = Redraw::default();
        let now = Instant::now();
        redraw.request();
        assert!(redraw.ready(now));
        redraw.drawn(now);
        for _ in 0..100 {
            redraw.request();
            assert!(!redraw.ready(now));
        }
        assert!(redraw.ready(now + FRAME_INTERVAL));
        redraw.drawn(now + FRAME_INTERVAL);
        assert!(!redraw.ready(now + FRAME_INTERVAL * 2));
    }
}
