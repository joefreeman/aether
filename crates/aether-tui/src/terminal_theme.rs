//! Whether the terminal is light or dark — what the `auto` theme follows here.
//!
//! Two questions, asked together once at startup behind one device-attributes fence:
//!
//! - `CSI ? 996 n`, the colour-scheme report (`CSI ? 997 ; 1 n` dark, `; 2 n` light). The
//!   terminal's own answer, so it wins when both come back. Ghostty, kitty, foot and contour
//!   answer it.
//! - `OSC 11 ; ?`, the default background colour, judged by its luminance. Older and much more
//!   widely answered — VTE (GNOME Terminal, Ptyxis) and xterm among them — and right as long as
//!   the terminal's palette follows its theme, which is what makes it light or dark at all.
//!
//! Asked once, not followed: mode 2031 would push a report whenever the terminal's theme changes,
//! but crossterm's parser has no case for `CSI ? … n` and would sit on one as an unfinished
//! sequence, swallowing keystrokes behind it.

use aether_client::theme::ThemeMode;

/// The colour-scheme query, then the background-colour query (ST-terminated, which every terminal
/// that answers accepts).
const QUERY: &[u8] = b"\x1b[?996n\x1b]11;?\x1b\\";

/// Ask the terminal. `None` when it answered neither question (or isn't a terminal). Raw mode must
/// be on and nothing else reading the tty — see [`crate::fenced_query`].
pub fn detect() -> Option<ThemeMode> {
    parse(&crate::fenced_query(QUERY))
}

/// The theme the terminal's replies declare: the colour-scheme report if there is one, else the
/// background colour's luminance.
fn parse(replies: &[u8]) -> Option<ThemeMode> {
    color_scheme_report(replies).or_else(|| background_report(replies).map(theme_of_background))
}

/// `CSI ? 997 ; Ps n`: 1 is dark, 2 is light.
fn color_scheme_report(replies: &[u8]) -> Option<ThemeMode> {
    const PREFIX: &[u8] = b"\x1b[?997;";
    let at = find(replies, PREFIX)? + PREFIX.len();
    match replies.get(at..at + 2)? {
        b"1n" => Some(ThemeMode::Dark),
        b"2n" => Some(ThemeMode::Light),
        _ => None,
    }
}

/// `OSC 11 ; rgb:R/G/B` (BEL- or ST-terminated) as linear-light `[r, g, b]` in `0.0..=1.0`. Each
/// component is 1–4 hex digits, scaled by its own width.
fn background_report(replies: &[u8]) -> Option<[f64; 3]> {
    const PREFIX: &[u8] = b"\x1b]11;";
    let start = find(replies, PREFIX)? + PREFIX.len();
    let rest = &replies[start..];
    let end = rest.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    let spec = std::str::from_utf8(&rest[..end]).ok()?;
    let mut parts = spec.strip_prefix("rgb:")?.split('/');
    let mut channel = || -> Option<f64> {
        let hex = parts.next()?;
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let max = (1u32 << (4 * hex.len())) - 1;
        let srgb = f64::from(u32::from_str_radix(hex, 16).ok()?) / f64::from(max);
        Some(if srgb <= 0.04045 {
            srgb / 12.92
        } else {
            ((srgb + 0.055) / 1.055).powf(2.4)
        })
    };
    let rgb = [channel()?, channel()?, channel()?];
    parts.next().is_none().then_some(rgb)
}

/// Dark below mid-grey: relative luminance 0.18 is CIE L* 50, perceptually halfway.
fn theme_of_background([r, g, b]: [f64; 3]) -> ThemeMode {
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    if luminance < 0.18 {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DA: &[u8] = b"\x1b[?62;22c";

    fn replies(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn the_colour_scheme_report_decides() {
        assert_eq!(
            parse(&replies(&[b"\x1b[?997;1n", DA])),
            Some(ThemeMode::Dark)
        );
        assert_eq!(
            parse(&replies(&[b"\x1b[?997;2n", DA])),
            Some(ThemeMode::Light)
        );
    }

    /// The terminal's own declaration beats a background we would only be judging.
    #[test]
    fn the_colour_scheme_report_beats_the_background() {
        let r = replies(&[b"\x1b[?997;2n", b"\x1b]11;rgb:0000/0000/0000\x1b\\", DA]);
        assert_eq!(parse(&r), Some(ThemeMode::Light));
    }

    /// What VTE answers: no colour-scheme report, only the background. Nord's dark ground and
    /// Ptyxis's light one, each in both terminators.
    #[test]
    fn the_background_decides_without_a_report() {
        let dark = replies(&[b"\x1b]11;rgb:2e2e/3434/4040\x1b\\", DA]);
        assert_eq!(parse(&dark), Some(ThemeMode::Dark));
        let light = replies(&[b"\x1b]11;rgb:fafa/fafa/fbfb\x07", DA]);
        assert_eq!(parse(&light), Some(ThemeMode::Light));
    }

    /// Components scale by their own width: `f/f/f` and `ffff/ffff/ffff` are both white.
    #[test]
    fn short_components_scale_to_their_width() {
        assert_eq!(parse(b"\x1b]11;rgb:f/f/f\x07"), Some(ThemeMode::Light));
        assert_eq!(parse(b"\x1b]11;rgb:20/20/20\x07"), Some(ThemeMode::Dark));
        // The threshold is perceptual, not the byte midpoint: L* 50 is about #767676, so a
        // #747474 ground is dark and a #7a7a7a one light.
        assert_eq!(
            parse(b"\x1b]11;rgb:7474/7474/7474\x07"),
            Some(ThemeMode::Dark)
        );
        assert_eq!(
            parse(b"\x1b]11;rgb:7a7a/7a7a/7a7a\x07"),
            Some(ThemeMode::Light)
        );
    }

    #[test]
    fn nothing_answered_is_no_answer() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(DA), None);
        // Malformed or unfinished replies don't guess.
        assert_eq!(parse(b"\x1b[?997;3n"), None);
        assert_eq!(parse(b"\x1b]11;rgb:2e2e/3434\x07"), None);
        assert_eq!(parse(b"\x1b]11;rgb:2e2e/3434/4040"), None);
        assert_eq!(parse(b"\x1b]11;#2e3440\x07"), None);
    }
}
