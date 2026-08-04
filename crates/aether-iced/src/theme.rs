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
pub const NORD3: Color = rgb(0x4c566a); // dim — hit/sneak fills, muted glyphs
pub const NORD3_BRIGHT: Color = rgb(0x616e88); // lighter dim (legible secondary text on panels)
/// Off-palette, brighter still — the syntax comment colour: NORD3 comments were ~1.4:1 against
/// the reading view's NORD1 code panels; this reaches ~2.8:1 there (~3.5:1 on the editor) while
/// staying below NORD9 keywords, so comments remain the dimmest rung of the ladder. Mirrors the
/// web's `--nord3-brighter`.
pub const NORD3_BRIGHTER: Color = rgb(0x7b88a1);
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

/// Scrollbar rail/thumb width for buffer-level and chrome scrollables — the editor's drawn bar,
/// the reading view's document scroll, pickers, dialogs, and popovers. One step heavier than
/// [`SCROLLBAR_INLINE_W`] so the "scrolls the view" bars outrank the in-content ones.
pub const SCROLLBAR_W: f32 = 4.0;
/// Thinner bar for panels *inside* content — the reading view's code blocks and tables, which
/// pan horizontally within the document rather than scrolling the view itself.
pub const SCROLLBAR_INLINE_W: f32 = 3.0;

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

/// Chrome sizing, derived from the `ui_font_size` app setting (`Space .`). Every size in the chrome
/// — status bar, pickers, dialogs, hover, toasts, hints — goes through here, so the whole UI scales
/// as one knob. The buffer text is *not* chrome: it has its own setting (`buffer_font_size`) that
/// the editor widget reads directly.
///
/// The hand-tuned literals the chrome was built from (13 body, 12 secondary, 14 heading, 24px picker
/// rows, …) are kept as sizes-at-the-default-base and scaled by [`Ui::at`], so the size hierarchy
/// survives at any base rather than collapsing into one flat size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ui {
    /// The setting's value in px. This *is* the body tier; every other tier is a ratio of it.
    base: f32,
}

/// The base the chrome literals were hand-tuned against — the denominator in [`Ui::at`]. Tied to the
/// setting's default, so a fresh install renders pixel-identical to the pre-setting chrome.
const TUNED_BASE: f32 = aether_protocol::settings::default_ui_font_size() as f32;

impl Ui {
    pub fn new(ui_font_size: u32) -> Self {
        Ui {
            base: ui_font_size as f32,
        }
    }

    /// Scale a dimension that was hand-tuned at [`TUNED_BASE`] to the current base. Used for text
    /// sizes and for the pixel dimensions that hold text (row heights, label columns, dialog
    /// widths) — pure whitespace (paddings, gaps, shadows) stays fixed, since it doesn't clip.
    pub fn at(self, tuned_px: f32) -> f32 {
        tuned_px * self.base / TUNED_BASE
    }

    /// Body text: picker rows, dialog copy, status-bar segments.
    pub fn body(self) -> f32 {
        self.base
    }

    /// Secondary text: counts, meta columns, descriptions, chips, the hint chip.
    pub fn small(self) -> f32 {
        self.at(12.0)
    }

    /// Fine print: the `(y)`/`(n)` key suffix on modal buttons.
    pub fn fine(self) -> f32 {
        self.at(11.0)
    }

    /// A `●` drawn as text inside a row (picker bullets), sized to read as a dot rather than a
    /// glyph.
    pub fn dot(self) -> f32 {
        self.at(9.0)
    }

    /// Dialog titles and section headings.
    pub fn heading(self) -> f32 {
        self.at(14.0)
    }

    /// A non-text control's box (the settings checkbox).
    pub fn control(self) -> f32 {
        self.at(16.0)
    }

    /// One line of chrome text including leading — what a stack of chrome lines measures per line
    /// (the hover popover's height estimate).
    pub fn line_height(self) -> f32 {
        self.at(19.0)
    }

    /// Rough px-per-character for chrome sans text, for the status bar's elide budget (chars
    /// approximate px there — see `status_bar`).
    pub fn char_width(self) -> f32 {
        self.at(6.5)
    }

    /// Every picker display row (item or group header) is exactly this tall — the unit the
    /// virtual-scroll spacer math is built on. Rounded to whole pixels so row boundaries stay crisp
    /// and the spacer arithmetic doesn't accumulate fractions.
    pub fn row_h(self) -> f32 {
        self.at(24.0).round()
    }
}

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
        "comment" => Some(NORD3_BRIGHTER),
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

    /// At the default `ui_font_size` the scale reproduces the literals the chrome was hand-tuned
    /// with — a fresh install must render exactly as it did before the setting existed.
    #[test]
    fn default_ui_scale_reproduces_the_tuned_sizes() {
        let ui = Ui::new(aether_protocol::settings::default_ui_font_size());
        assert_eq!(ui.body(), 13.0);
        assert_eq!(ui.small(), 12.0);
        assert_eq!(ui.fine(), 11.0);
        assert_eq!(ui.dot(), 9.0);
        assert_eq!(ui.heading(), 14.0);
        assert_eq!(ui.control(), 16.0);
        assert_eq!(ui.row_h(), 24.0);
    }

    /// Every tier scales with the setting — including the picker's row height, which holds the row
    /// text (a row that didn't grow would clip it) — and the hierarchy between tiers survives.
    #[test]
    fn ui_scale_grows_with_the_setting() {
        let ui = Ui::new(26); // twice the default base
        assert_eq!(ui.body(), 26.0);
        assert_eq!(ui.small(), 24.0);
        assert_eq!(ui.row_h(), 48.0);
        assert_eq!(
            ui.at(720.0),
            1440.0,
            "panel widths track the text they hold"
        );
        assert!(ui.dot() < ui.fine() && ui.fine() < ui.small());
        assert!(ui.small() < ui.body() && ui.body() < ui.heading());

        // A smaller base shrinks the same way, and rounding keeps row boundaries on whole pixels.
        let ui = Ui::new(11);
        assert!(ui.body() < 13.0);
        assert_eq!(ui.row_h(), (11.0 * 24.0 / 13.0_f32).round());
        assert_eq!(ui.row_h().fract(), 0.0);
    }
}
