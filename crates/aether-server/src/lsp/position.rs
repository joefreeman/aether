//! Conversion between Aether's internal columns and LSP `character` offsets.
//!
//! Aether is UTF-8 throughout: ropey stores UTF-8 and the protocol's `col` is a *byte* offset into
//! a line's UTF-8 (see `docs/protocol.md` §3). LSP positions instead count code units in a
//! *negotiated* encoding — UTF-16 by default, but UTF-8 (and UTF-32) are negotiable via the
//! `positionEncoding` capability (LSP 3.17). We advertise UTF-8 first and fall back. This module is
//! the single place the two coordinate systems meet, so nothing else in the server has to think
//! about UTF-16.
//!
//! These functions operate on one line's text, which must exclude the trailing newline (LSP
//! positions never index the line terminator). The caller pairs the column with a line number to
//! form a full position.

/// The position encoding negotiated with a language server. Variants map to the LSP
/// `PositionEncodingKind` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    /// `character` counts UTF-8 bytes — identical to Aether's internal byte columns.
    Utf8,
    /// `character` counts UTF-16 code units (the LSP default).
    Utf16,
    /// `character` counts Unicode scalar values (`char`s).
    Utf32,
}

impl PositionEncoding {
    /// Parse an LSP `PositionEncodingKind`. Unknown values fall back to UTF-16, the encoding every
    /// server is required to support.
    pub fn from_lsp(kind: &str) -> Self {
        match kind {
            "utf-8" => Self::Utf8,
            "utf-32" => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    pub fn as_lsp(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }
}

/// Convert a byte column within `line` to an LSP `character` offset in `encoding`.
///
/// `byte_col` is clamped to the line length and floored to a char boundary, so a column pointing
/// into the middle of a multi-byte char (which real cursor positions never do) maps to that char's
/// start rather than panicking.
pub fn byte_to_lsp(line: &str, byte_col: usize, encoding: PositionEncoding) -> u32 {
    let byte_col = floor_char_boundary(line, byte_col);
    match encoding {
        PositionEncoding::Utf8 => byte_col as u32,
        PositionEncoding::Utf16 => line[..byte_col].chars().map(|c| c.len_utf16() as u32).sum(),
        PositionEncoding::Utf32 => line[..byte_col].chars().count() as u32,
    }
}

/// Convert an LSP `character` offset within `line` to a byte column.
///
/// A `character` past the end of the line clamps to the line's byte length. A `character` that
/// would land inside a multi-unit char (e.g. between the halves of a UTF-16 surrogate pair) clamps
/// to that char's start — defensive; conformant servers don't emit such positions.
pub fn lsp_to_byte(line: &str, character: u32, encoding: PositionEncoding) -> usize {
    let character = character as usize;
    match encoding {
        PositionEncoding::Utf8 => floor_char_boundary(line, character),
        PositionEncoding::Utf16 => count_to_byte(line, character, |c| c.len_utf16()),
        PositionEncoding::Utf32 => count_to_byte(line, character, |_| 1),
    }
}

/// Walk `line`, summing `units(char)` per char, and return the byte offset at which the running
/// total reaches `target`. Lands on the start of the char that would cross `target`.
fn count_to_byte(line: &str, target: usize, units: impl Fn(char) -> usize) -> usize {
    let mut total = 0usize;
    for (byte_idx, c) in line.char_indices() {
        if total >= target || total + units(c) > target {
            return byte_idx;
        }
        total += units(c);
    }
    line.len()
}

/// `index` rounded down to the nearest char boundary (`std::str::floor_char_boundary` is unstable).
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PositionEncoding; 3] = [
        PositionEncoding::Utf8,
        PositionEncoding::Utf16,
        PositionEncoding::Utf32,
    ];

    #[test]
    fn ascii_is_identity_in_every_encoding() {
        let line = "hello";
        for enc in ALL {
            assert_eq!(byte_to_lsp(line, 0, enc), 0);
            assert_eq!(byte_to_lsp(line, 3, enc), 3);
            assert_eq!(byte_to_lsp(line, 5, enc), 5);
            assert_eq!(lsp_to_byte(line, 3, enc), 3);
        }
    }

    #[test]
    fn two_byte_char() {
        // "é" is 2 UTF-8 bytes, 1 UTF-16 unit, 1 scalar. Byte col of 'b' is 3.
        let line = "aéb";
        assert_eq!(byte_to_lsp(line, 3, PositionEncoding::Utf8), 3);
        assert_eq!(byte_to_lsp(line, 3, PositionEncoding::Utf16), 2);
        assert_eq!(byte_to_lsp(line, 3, PositionEncoding::Utf32), 2);
        // 'b' sits at byte 3; reversing the offsets above lands back on it. In UTF-8 `character`
        // *is* the byte offset, so 'b' is character 3 there, but character 2 in UTF-16/UTF-32.
        assert_eq!(lsp_to_byte(line, 3, PositionEncoding::Utf8), 3);
        assert_eq!(lsp_to_byte(line, 2, PositionEncoding::Utf16), 3);
        assert_eq!(lsp_to_byte(line, 2, PositionEncoding::Utf32), 3);
    }

    #[test]
    fn astral_char_is_surrogate_pair_in_utf16() {
        // "😀" is 4 UTF-8 bytes, 2 UTF-16 units, 1 scalar. Byte col of 'y' is 5.
        let line = "x😀y";
        assert_eq!(byte_to_lsp(line, 5, PositionEncoding::Utf8), 5);
        assert_eq!(byte_to_lsp(line, 5, PositionEncoding::Utf16), 3);
        assert_eq!(byte_to_lsp(line, 5, PositionEncoding::Utf32), 2);
        assert_eq!(lsp_to_byte(line, 3, PositionEncoding::Utf16), 5);
        assert_eq!(lsp_to_byte(line, 2, PositionEncoding::Utf32), 5);
    }

    #[test]
    fn clamps_past_end_of_line() {
        let line = "ab";
        for enc in ALL {
            assert_eq!(byte_to_lsp(line, 99, enc), 2);
            assert_eq!(lsp_to_byte(line, 99, enc), 2);
        }
    }

    #[test]
    fn character_inside_a_char_clamps_to_its_start() {
        // UTF-16 character 1 falls between 😀's two surrogate halves → start of the char.
        assert_eq!(lsp_to_byte("😀", 1, PositionEncoding::Utf16), 0);
        // A byte col mid-"é" floors to the char start.
        assert_eq!(byte_to_lsp("é", 1, PositionEncoding::Utf16), 0);
    }

    #[test]
    fn roundtrips_at_every_char_boundary() {
        let line = "aé😀b☃c";
        for enc in ALL {
            let boundaries = line
                .char_indices()
                .map(|(b, _)| b)
                .chain(std::iter::once(line.len()));
            for byte_idx in boundaries {
                let lsp = byte_to_lsp(line, byte_idx, enc);
                assert_eq!(
                    lsp_to_byte(line, lsp, enc),
                    byte_idx,
                    "enc={enc:?} byte={byte_idx}"
                );
            }
        }
    }

    #[test]
    fn encoding_parse_and_render() {
        for (s, enc) in [
            ("utf-8", PositionEncoding::Utf8),
            ("utf-16", PositionEncoding::Utf16),
            ("utf-32", PositionEncoding::Utf32),
        ] {
            assert_eq!(PositionEncoding::from_lsp(s), enc);
            assert_eq!(enc.as_lsp(), s);
        }
        assert_eq!(PositionEncoding::from_lsp("banana"), PositionEncoding::Utf16);
    }
}
