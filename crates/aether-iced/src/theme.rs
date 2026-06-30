//! Nord palette — mirrors `web/src/theme.css` and `aether-tui/src/ui.rs` so all clients match.

use iced::Color;

const fn rgb(hex: u32) -> Color {
    Color {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

pub const NORD0: Color = rgb(0x2e3440); // main background
pub const NORD1: Color = rgb(0x3b4252); // status line / panel
pub const NORD2: Color = rgb(0x434c5e); // picker row highlight / chips
pub const NORD3: Color = rgb(0x4c566a); // comments / dim
pub const NORD3_BRIGHT: Color = rgb(0x616e88); // lighter dim (legible secondary text on panels)
pub const NORD4: Color = rgb(0xd8dee9); // main foreground
pub const NORD6: Color = rgb(0xeceff4); // brightest text (search query, file label)
pub const NORD7: Color = rgb(0x8fbcbb); // types
pub const NORD8: Color = rgb(0x88c0d0); // functions, accents
pub const NORD9: Color = rgb(0x81a1c1); // keywords, operators
pub const NORD10: Color = rgb(0x5e81ac); // Frost — deep blue (active selection bg)
pub const NORD11: Color = rgb(0xbf616a); // error
pub const NORD12: Color = rgb(0xd08770); // attributes, macros
pub const NORD13: Color = rgb(0xebcb8b); // string escapes, warnings
pub const NORD14: Color = rgb(0xa3be8c); // strings
pub const NORD15: Color = rgb(0xb48ead); // numbers, constants

/// Sneak typed-prefix band — a brighter, cooler slate than the word tint (NORD3), between it and
/// the bright label cell in prominence.
pub const SNEAK_PREFIX_BG: Color = rgb(0x616e88);

/// Current-line tint — between NORD0 and NORD1 (see theme.css for the rationale).
pub const CURSOR_LINE_BG: Color = rgb(0x343a48);

// Gutter change-bar colours (hue says what changed; dim variants mean "staged").
pub const GIT_ADDED: Color = NORD14;
pub const GIT_MODIFIED: Color = NORD13;
pub const GIT_DELETED: Color = NORD11;
pub const GIT_STAGED_ADDED: Color = rgb(0x6e8060);
pub const GIT_STAGED_MODIFIED: Color = rgb(0x9e8a62);
pub const GIT_STAGED_DELETED: Color = rgb(0x844c53);

// Inline-diff line tints (and the phantom deleted rows' backgrounds), bright vs staged-dim.
pub const GIT_ADDED_BG: Color = rgb(0x2d3a2d);
pub const GIT_MODIFIED_BG: Color = rgb(0x3a3628);
pub const GIT_DELETED_BG: Color = rgb(0x3b2226);
pub const GIT_STAGED_ADDED_BG: Color = rgb(0x2f3631);
pub const GIT_STAGED_MODIFIED_BG: Color = rgb(0x35342d);
pub const GIT_STAGED_DELETED_BG: Color = rgb(0x33252a);

// Cursor-line variants on changed lines under the diff view, so the cursorline doesn't hide
// the change colour.
pub const CURSOR_LINE_ADDED_BG: Color = rgb(0x3a4d3a);
pub const CURSOR_LINE_MODIFIED_BG: Color = rgb(0x4a4632);
pub const CURSOR_LINE_STAGED_ADDED_BG: Color = rgb(0x3a453c);
pub const CURSOR_LINE_STAGED_MODIFIED_BG: Color = rgb(0x434138);

/// Tree-sitter highlight kind → colour. Mirrors `render.ts::HL_CLASS` + theme.css (and
/// `ui.rs::lookup_exact`). Unlisted kinds fall back by stripping trailing `.segments`
/// (`"function.call"` → `"function"`); `None` means "default foreground".
pub fn highlight_color(kind: &str) -> Option<Color> {
    let mut k = kind;
    loop {
        if let Some(c) = lookup_exact(k) {
            return c;
        }
        match k.rfind('.') {
            Some(dot) => k = &k[..dot],
            None => return None,
        }
    }
}

fn lookup_exact(kind: &str) -> Option<Option<Color>> {
    Some(match kind {
        "keyword" | "variable.builtin" | "operator" | "tag" => Some(NORD9),
        "string" | "text.literal" => Some(NORD14),
        "string.escape" | "string.special" => Some(NORD13),
        "comment" => Some(NORD3),
        "number" | "boolean" | "constant" | "constant.builtin" => Some(NORD15),
        "function" | "function.call" | "text.title" | "text.uri" | "text.reference" => Some(NORD8),
        "function.macro" | "punctuation.special" | "attribute" | "label" => Some(NORD12),
        "type" | "type.builtin" | "module" | "namespace" | "constructor" => Some(NORD7),
        "variable.parameter" | "punctuation.bracket" | "punctuation.delimiter" | "property" => {
            Some(NORD4)
        }
        "text.emphasis" | "text.strong" => None,
        _ => return None,
    })
}

pub fn diagnostic_color(severity: aether_protocol::viewport::DiagnosticSeverity) -> Color {
    use aether_protocol::viewport::DiagnosticSeverity as S;
    match severity {
        S::Error => NORD11,
        S::Warning => NORD13,
        S::Information => NORD8,
        // Near-white, not a hue: readable on the dark popover/status backgrounds and distinct
        // from the coloured severities (was NORD8, which made it indistinguishable from info).
        S::Hint => NORD4,
    }
}

/// Severity glyph for the status-bar count, diagnostics picker, and hover popover, so all three
/// native surfaces match. Refined Unicode approximations of the web client's icons (circled ✕ /
/// warning triangle / circled i); Hint is a hollow circle `○`.
pub fn diag_glyph(severity: aether_protocol::viewport::DiagnosticSeverity) -> &'static str {
    use aether_protocol::viewport::DiagnosticSeverity as S;
    match severity {
        S::Error => "⊗",
        S::Warning => "⚠",
        S::Information => "ⓘ",
        S::Hint => "○",
    }
}

/// State colour for a language-server's status dot — mirrors `ui.rs::lsp_status_color` (and the
/// web client's icon classes). A ready server with in-flight `$/progress` shows the busy colour;
/// the caller checks `progress`.
pub fn lsp_status_color(status: &aether_protocol::lsp::LspStatus) -> Color {
    use aether_protocol::lsp::LspStatus as S;
    match status {
        S::Ready => NORD14,
        S::Starting | S::Initializing | S::Restarting => NORD13,
        S::Crashed { .. } => NORD11,
        S::Stopped => NORD3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_strips_dotted_suffixes() {
        // "function.method.call" isn't listed; it should fall back to "function".
        assert_eq!(
            highlight_color("function.method.call"),
            highlight_color("function")
        );
        assert!(highlight_color("function").is_some());
        // Unknown kinds resolve to the default foreground.
        assert_eq!(highlight_color("nonsense"), None);
        // Emphasis is listed but has no colour of its own.
        assert_eq!(highlight_color("text.emphasis"), None);
    }
}
