//! Shared vertical-scrollbar thumb geometry.
//!
//! The single source of truth for "where does the thumb sit, and how long is it" — used by
//! every shell's scrollbar so the three implementations don't drift. It's purely arithmetic
//! and unit-agnostic: the TUI passes cell counts, the iced shell passes pixels. The web shell
//! uses the browser's native scrollbar and doesn't call this.

/// Thumb `(start, length)` along a track of `track` units, for a viewport of `viewport`
/// units showing content of `total` units, scrolled `offset` units from the top.
///
/// `min_len` floors the thumb so it stays visible/grabbable on very long content. Returns
/// `None` when the content fits (`total <= viewport`) or the track is empty — the caller
/// hides the bar.
///
/// The math: the thumb covers the same fraction of the track as the viewport covers of the
/// content (`viewport / total`), positioned proportionally to the scroll offset
/// (`offset / total`), then clamped so it never spills past the track end.
pub fn thumb(
    track: f64,
    total: f64,
    viewport: f64,
    offset: f64,
    min_len: f64,
) -> Option<(f64, f64)> {
    if track <= 0.0 || total <= 0.0 || viewport >= total {
        return None;
    }
    let len = (viewport / total * track).max(min_len).min(track);
    let max_start = track - len;
    let start = (offset / total * track).clamp(0.0, max_start);
    Some((start, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn fits_in_viewport_means_no_bar() {
        assert_eq!(thumb(100.0, 20.0, 20.0, 0.0, 1.0), None);
        assert_eq!(thumb(100.0, 10.0, 50.0, 0.0, 1.0), None);
    }

    #[test]
    fn empty_track_means_no_bar() {
        assert_eq!(thumb(0.0, 100.0, 10.0, 0.0, 1.0), None);
    }

    #[test]
    fn at_top_thumb_starts_at_zero() {
        let (start, len) = thumb(100.0, 200.0, 50.0, 0.0, 1.0).unwrap();
        approx(start, 0.0);
        approx(len, 25.0); // 50/200 * 100
    }

    #[test]
    fn at_bottom_thumb_is_flush_with_track_end() {
        // offset = total - viewport = 150; raw start = 75, but clamped to track - len = 75.
        let (start, len) = thumb(100.0, 200.0, 50.0, 150.0, 1.0).unwrap();
        approx(len, 25.0);
        approx(start, 75.0);
        approx(start + len, 100.0);
    }

    #[test]
    fn overshooting_offset_is_clamped_to_track_end() {
        let (start, len) = thumb(100.0, 200.0, 50.0, 99999.0, 1.0).unwrap();
        approx(start, 100.0 - len);
    }

    #[test]
    fn min_len_floors_a_tiny_thumb() {
        // 10 visible of 100000 → raw len 0.01, floored to min_len.
        let (_, len) = thumb(100.0, 100_000.0, 10.0, 0.0, 1.0).unwrap();
        approx(len, 1.0);
    }

    #[test]
    fn midway_offset_is_proportional() {
        // Half-scrolled content puts the thumb start at half the track (minus clamp room).
        let (start, len) = thumb(100.0, 400.0, 100.0, 200.0, 1.0).unwrap();
        approx(len, 25.0);
        approx(start, 50.0);
    }
}
