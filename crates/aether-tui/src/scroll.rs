//! Vertical scroll state for static, client-rendered lists (e.g. the hover popup).
//!
//! The wrinkle this solves: input handling (key / wheel) needs to clamp the offset to the real
//! maximum — `content_lines - viewport_rows` — but only the *render* path knows the viewport
//! height. Previously the offset was bumped in the key handler and only clamped at draw time, so
//! the stored value could run far past the bottom; scrolling back up then had to unwind that
//! invisible overshoot (a "hidden cursor" feel).
//!
//! So the renderer records the geometry each frame (interior-mutable, since it only holds `&self`)
//! and the input handlers read it back when clamping. The box geometry stays owned by the renderer;
//! input handling just consumes the last-known metrics. Generic over content, so any future static
//! overlay can reuse it.

use std::cell::Cell;

/// Scrollable viewport over a fixed list of `content` lines showing `viewport` rows at a time.
/// `offset` is the index of the top visible line.
#[derive(Debug, Default)]
pub struct ScrollState {
    offset: u16,
    // Last-rendered geometry. `Cell` so `record` can run on the immutable draw path.
    content: Cell<u16>,
    viewport: Cell<u16>,
}

impl ScrollState {
    /// Record the latest geometry (total content lines, visible rows). Called once per render.
    pub fn record(&self, content: u16, viewport: u16) {
        self.content.set(content);
        self.viewport.set(viewport);
    }

    /// The furthest the top line can scroll while keeping the last screen of content in view.
    fn max_offset(&self) -> u16 {
        self.content.get().saturating_sub(self.viewport.get())
    }

    /// The clamped offset to render from. Reading clamps too, so a stale offset after a resize
    /// (smaller viewport) still renders in-bounds.
    pub fn offset(&self) -> u16 {
        self.offset.min(self.max_offset())
    }

    /// Scroll by `delta` rows (negative = up), clamped to the scrollable range.
    pub fn scroll_by(&mut self, delta: i32) {
        let max = i32::from(self.max_offset());
        self.offset = (i32::from(self.offset) + delta).clamp(0, max) as u16;
    }

    /// Page up/down, keeping one row of overlap for orientation.
    pub fn page(&mut self, down: bool) {
        let step = i32::from(self.viewport.get().saturating_sub(1).max(1));
        self.scroll_by(if down { step } else { -step });
    }

    /// Scroll by half the viewport (the editor's `ScrollUnit::Half`), for Alt-Up/Down.
    pub fn half(&mut self, down: bool) {
        let step = i32::from((self.viewport.get() / 2).max(1));
        self.scroll_by(if down { step } else { -step });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_recorded_geometry() {
        let mut s = ScrollState::default();
        s.record(100, 20); // 100 lines, 20 visible → max offset 80.
        s.scroll_by(1000);
        assert_eq!(s.offset(), 80, "cannot scroll past the last screen");
        s.scroll_by(-1000);
        assert_eq!(s.offset(), 0, "cannot scroll above the top");
    }

    #[test]
    fn no_overshoot_to_unwind() {
        // The regression we're guarding: pushing down at the bottom must not bank hidden offset
        // that then has to be unwound before the view moves back up.
        let mut s = ScrollState::default();
        s.record(30, 10); // max offset 20.
        for _ in 0..50 {
            s.scroll_by(1);
        }
        assert_eq!(s.offset(), 20);
        s.scroll_by(-1);
        assert_eq!(s.offset(), 19, "one step up moves immediately");
    }

    #[test]
    fn fits_in_viewport_means_no_scroll() {
        let mut s = ScrollState::default();
        s.record(5, 20); // content shorter than viewport.
        s.scroll_by(10);
        assert_eq!(s.offset(), 0);
    }

    #[test]
    fn page_respects_bounds() {
        let mut s = ScrollState::default();
        s.record(100, 10); // page step = 9, max offset 90.
        s.page(true);
        assert_eq!(s.offset(), 9);
        s.scroll_by(1000); // jump to the bottom
        s.page(true);
        assert_eq!(s.offset(), 90);
        s.page(false);
        assert_eq!(s.offset(), 81);
    }
}
