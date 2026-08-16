//! The iced shell's palette: the client core's role table ([`aether_client::theme::Theme`])
//! converted to [`iced::Color`] once per mode. Call sites reference *roles* (`p.bg`, `p.accent`)
//! rather than Nord shades, so the light theme is the same code path with a different table —
//! the core owns both tables and the semantic mappings (syntax kind / diagnostic severity /
//! LSP status → role); this module is only the `Rgb → Color` edge.

use aether_client::theme::Theme;
use aether_protocol::settings::ThemeMode;
use iced::Color;
use std::sync::LazyLock;

/// Convert a core palette colour to iced's colour type — the one place `Rgb` crosses into iced.
fn color(c: aether_client::theme::Rgb) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

/// One theme's colour roles as `iced::Color` — field-for-field the core's [`Theme`] (minus the
/// `syn_*` syntax roles, which only flow through [`highlight_color`]). Resolve once per view pass
/// with [`palette`] and thread it alongside [`Ui`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// The mode this table renders — for the rare mode branches (the base widget theme,
    /// [`highlight_color`] at draw time).
    pub mode: ThemeMode,

    // ---- Backgrounds ----
    pub bg: Color,
    pub bg_panel: Color,
    pub bg_selection: Color,
    pub bg_visual: Color,
    pub fill_dim: Color,
    pub sneak_prefix_bg: Color,
    pub match_highlight: Color,
    pub match_bracket: Color,
    pub cursor_line_bg: Color,
    pub overlay_border: Color,
    pub border_subtle: Color,

    // ---- Foregrounds ----
    pub fg: Color,
    pub fg_bright: Color,
    pub fg_muted: Color,
    pub fg_dim: Color,
    pub fg_faint: Color,
    pub fg_on_accent: Color,

    // ---- Accents & status ----
    pub accent: Color,
    pub accent_alt: Color,
    pub accent_deep: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub ok: Color,

    // ---- Buffer-state dot ----
    pub state_deleted: Color,
    pub state_changed: Color,
    pub state_unsaved: Color,

    // ---- Git ----
    pub git_added: Color,
    pub git_modified: Color,
    pub git_deleted: Color,
    pub git_staged_added: Color,
    pub git_staged_modified: Color,
    pub git_staged_deleted: Color,
    pub git_added_bg: Color,
    pub git_modified_bg: Color,
    pub git_deleted_bg: Color,
    pub git_staged_added_bg: Color,
    pub git_staged_modified_bg: Color,
    pub git_staged_deleted_bg: Color,
    pub git_modified_emph_bg: Color,
    pub git_deleted_emph_bg: Color,
    pub git_staged_modified_emph_bg: Color,
    pub git_staged_deleted_emph_bg: Color,
    pub cursor_line_added_bg: Color,
    pub cursor_line_modified_bg: Color,
    pub cursor_line_staged_added_bg: Color,
    pub cursor_line_staged_modified_bg: Color,

    // ---- Markdown reading view ----
    pub md_code_bg: Color,
    pub md_table_stripe_bg: Color,
    pub md_alert_important: Color,
}

impl Palette {
    fn from_theme(t: &Theme) -> Palette {
        Palette {
            mode: t.mode,
            bg: color(t.bg),
            bg_panel: color(t.bg_panel),
            bg_selection: color(t.bg_selection),
            bg_visual: color(t.bg_visual),
            fill_dim: color(t.fill_dim),
            sneak_prefix_bg: color(t.sneak_prefix_bg),
            match_highlight: color(t.match_highlight),
            match_bracket: color(t.match_bracket),
            cursor_line_bg: color(t.cursor_line_bg),
            overlay_border: color(t.overlay_border),
            border_subtle: color(t.border_subtle),
            fg: color(t.fg),
            fg_bright: color(t.fg_bright),
            fg_muted: color(t.fg_muted),
            fg_dim: color(t.fg_dim),
            fg_faint: color(t.fg_faint),
            fg_on_accent: color(t.fg_on_accent),
            accent: color(t.accent),
            accent_alt: color(t.accent_alt),
            accent_deep: color(t.accent_deep),
            error: color(t.error),
            warning: color(t.warning),
            info: color(t.info),
            ok: color(t.ok),
            state_deleted: color(t.state_deleted),
            state_changed: color(t.state_changed),
            state_unsaved: color(t.state_unsaved),
            git_added: color(t.git_added),
            git_modified: color(t.git_modified),
            git_deleted: color(t.git_deleted),
            git_staged_added: color(t.git_staged_added),
            git_staged_modified: color(t.git_staged_modified),
            git_staged_deleted: color(t.git_staged_deleted),
            git_added_bg: color(t.git_added_bg),
            git_modified_bg: color(t.git_modified_bg),
            git_deleted_bg: color(t.git_deleted_bg),
            git_staged_added_bg: color(t.git_staged_added_bg),
            git_staged_modified_bg: color(t.git_staged_modified_bg),
            git_staged_deleted_bg: color(t.git_staged_deleted_bg),
            git_modified_emph_bg: color(t.git_modified_emph_bg),
            git_deleted_emph_bg: color(t.git_deleted_emph_bg),
            git_staged_modified_emph_bg: color(t.git_staged_modified_emph_bg),
            git_staged_deleted_emph_bg: color(t.git_staged_deleted_emph_bg),
            cursor_line_added_bg: color(t.cursor_line_added_bg),
            cursor_line_modified_bg: color(t.cursor_line_modified_bg),
            cursor_line_staged_added_bg: color(t.cursor_line_staged_added_bg),
            cursor_line_staged_modified_bg: color(t.cursor_line_staged_modified_bg),
            md_code_bg: color(t.md_code_bg),
            md_table_stripe_bg: color(t.md_table_stripe_bg),
            md_alert_important: color(t.md_alert_important),
        }
    }
}

/// The role table for a mode, converted once and cached — resolve at the top of a view pass
/// (`theme::palette(self.session.theme)`) and pass `&Palette` down like [`Ui`].
pub fn palette(mode: ThemeMode) -> &'static Palette {
    static DARK: LazyLock<Palette> =
        LazyLock::new(|| Palette::from_theme(Theme::of(ThemeMode::Dark)));
    static LIGHT: LazyLock<Palette> =
        LazyLock::new(|| Palette::from_theme(Theme::of(ThemeMode::Light)));
    match mode {
        ThemeMode::Dark => &DARK,
        ThemeMode::Light => &LIGHT,
    }
}

/// The base iced widget theme for a mode — what theme-inheriting surfaces (markdown hover body
/// text, scrollbar chrome) draw from. Dark keeps the built-in `Nord` theme so its derived chrome
/// stays bit-identical to the pre-theme app; light generates a custom theme from the core light
/// table's anchor roles, so inherited chrome matches the palette instead of iced's default Light.
pub fn base_iced_theme(mode: ThemeMode) -> iced::Theme {
    match mode {
        ThemeMode::Dark => iced::Theme::Nord,
        ThemeMode::Light => {
            static LIGHT: LazyLock<iced::Theme> = LazyLock::new(|| {
                let t = Theme::of(ThemeMode::Light);
                iced::Theme::custom(
                    "Aether Light",
                    iced::theme::Palette {
                        background: color(t.bg),
                        text: color(t.fg),
                        primary: color(t.accent),
                        success: color(t.ok),
                        warning: color(t.warning),
                        danger: color(t.error),
                    },
                )
            });
            LIGHT.clone()
        }
    }
}

/// Scrollbar rail/thumb width for buffer-level and chrome scrollables — the editor's drawn bar,
/// the reading view's document scroll, pickers, dialogs, and popovers. One step heavier than
/// [`SCROLLBAR_INLINE_W`] so the "scrolls the view" bars outrank the in-content ones.
pub const SCROLLBAR_W: f32 = 4.0;
/// Thinner bar for panels *inside* content — the reading view's code blocks and tables, which
/// pan horizontally within the document rather than scrolling the view itself.
pub const SCROLLBAR_INLINE_W: f32 = 3.0;

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

    /// A `●` drawn as text inside a row (the git-status picker bullets), sized to read as a
    /// dot rather than a glyph.
    pub fn dot(self) -> f32 {
        self.at(9.0)
    }

    /// The buffer-state `●` — one indicator, one size: the status bar's dirty dot and the
    /// buffer/workspace picker rows' trailing dot all draw at this (deliberately the body
    /// size — the glyph's ink reads as a dot at text scale), so they can't drift.
    pub fn state_dot(self) -> f32 {
        self.at(13.0)
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

/// Tree-sitter highlight kind → colour, via the core's [`Theme::syntax`] (dotted-prefix fallback
/// included). `None` means "default foreground". Font attributes (bold/italic for markdown
/// strong/emphasis) stay keyed on the kind name in `editor.rs::highlight_font`; this returns the
/// colour only.
pub fn highlight_color(mode: ThemeMode, kind: &str) -> Option<Color> {
    Theme::of(mode)
        .syntax(kind)
        .and_then(|s| s.color)
        .map(color)
}

/// The underline / message colour for a diagnostic severity, via the core's [`Theme::diagnostic`]
/// (Hint is the plain foreground, not a hue).
pub fn diagnostic_color(
    mode: ThemeMode,
    severity: aether_protocol::viewport::DiagnosticSeverity,
) -> Color {
    color(Theme::of(mode).diagnostic(severity))
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

/// State colour for a language-server's status dot, via the core's [`Theme::lsp_status`]. A ready
/// server with in-flight `$/progress` shows the busy colour; the caller checks `progress`.
pub fn lsp_status_color(mode: ThemeMode, status: &aether_protocol::lsp::LspStatus) -> Color {
    color(Theme::of(mode).lsp_status(status))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(hex: u32) -> Color {
        Color::from_rgb8(
            ((hex >> 16) & 0xff) as u8,
            ((hex >> 8) & 0xff) as u8,
            (hex & 0xff) as u8,
        )
    }

    /// The dark palette must stay pixel-identical to the Nord constants this shell carried before
    /// the role table existed.
    #[test]
    fn dark_palette_pins_the_pre_theme_constants() {
        let p = palette(ThemeMode::Dark);
        assert_eq!(p.mode, ThemeMode::Dark);
        assert_eq!(p.bg, c(0x2e3440)); // NORD0
        assert_eq!(p.bg_panel, c(0x3b4252)); // NORD1
        assert_eq!(p.bg_selection, c(0x434c5e)); // NORD2
        assert_eq!(p.bg_visual, c(0x5e81ac)); // NORD10
        assert_eq!(p.fill_dim, c(0x4c566a)); // NORD3
        assert_eq!(p.sneak_prefix_bg, c(0x616e88)); // SNEAK_PREFIX_BG
        assert_eq!(p.cursor_line_bg, c(0x343a48)); // CURSOR_LINE_BG
        assert_eq!(p.fg, c(0xd8dee9)); // NORD4
        assert_eq!(p.fg_bright, c(0xeceff4)); // NORD6
        assert_eq!(p.fg_dim, c(0x616e88)); // NORD3_BRIGHT
        assert_eq!(p.fg_faint, c(0x4c566a)); // NORD3
        assert_eq!(p.fg_on_accent, c(0x2e3440)); // NORD0
        assert_eq!(p.accent, c(0x88c0d0)); // NORD8
        assert_eq!(p.accent_alt, c(0x81a1c1)); // NORD9
        assert_eq!(p.accent_deep, c(0x5e81ac)); // NORD10
        assert_eq!(p.error, c(0xbf616a)); // NORD11
        assert_eq!(p.warning, c(0xebcb8b)); // NORD13
        assert_eq!(p.ok, c(0xa3be8c)); // NORD14
                                       // The roles that share a dark shade with a semantically unrelated role — pinned so the
                                       // rename from the borrowed role can never shift dark rendering.
        assert_eq!(p.match_highlight, c(0xebcb8b)); // NORD13 (= warning's dark shade)
        assert_eq!(p.match_bracket, c(0xd08770)); // NORD12 (= syn_macro's dark shade)
        assert_eq!(p.md_alert_important, c(0xb48ead)); // NORD15 (= syn_constant's dark shade)
        assert_eq!(p.fg_muted, c(0x7b88a1)); // NORD3_BRIGHTER (= overlay_border's dark shade)
        assert_eq!(p.border_subtle, c(0x4c566a)); // NORD3 (= fill_dim's dark shade)
        assert_eq!(p.git_added, c(0xa3be8c)); // GIT_ADDED
        assert_eq!(p.git_staged_added, c(0x6e8060)); // GIT_STAGED_ADDED
        assert_eq!(p.git_staged_modified, c(0x9e8a62)); // GIT_STAGED_MODIFIED
        assert_eq!(p.git_staged_deleted, c(0x844c53)); // GIT_STAGED_DELETED
        assert_eq!(p.git_added_bg, c(0x2d3a2d)); // GIT_ADDED_BG
        assert_eq!(p.cursor_line_added_bg, c(0x3a4d3a)); // CURSOR_LINE_ADDED_BG
        assert_eq!(p.cursor_line_staged_modified_bg, c(0x434138));
        assert_eq!(p.md_code_bg, c(0x3b4252)); // reading-view code panel (NORD1)
    }

    /// Light is a real second table, not dark re-served.
    #[test]
    fn light_palette_is_distinct() {
        let d = palette(ThemeMode::Dark);
        let l = palette(ThemeMode::Light);
        assert_eq!(l.mode, ThemeMode::Light);
        assert_ne!(l.bg, d.bg);
        assert_ne!(l.fg, d.fg);
        assert_ne!(l.warning, d.warning, "aurora yellow is unreadable on light");
        assert_eq!(l.bg, c(0xeceff4)); // NORD6 — the ends swap
        assert_eq!(l.fg, c(0x2e3440)); // NORD0
    }

    /// The mapping fns delegate to the core tables: dark answers must be the historic constants.
    #[test]
    fn mapping_fns_delegate_to_the_core() {
        use aether_protocol::lsp::LspStatus;
        use aether_protocol::viewport::DiagnosticSeverity as S;
        let dark = ThemeMode::Dark;
        // Dotted fallback lives in the core; the wrapper just converts.
        assert_eq!(
            highlight_color(dark, "function.method.call"),
            highlight_color(dark, "function")
        );
        assert_eq!(highlight_color(dark, "function"), Some(c(0x88c0d0))); // NORD8
        assert_eq!(highlight_color(dark, "comment"), Some(c(0x7b88a1))); // NORD3_BRIGHTER
        assert_eq!(highlight_color(dark, "nonsense"), None);
        // Emphasis carries attributes only — no colour, so the default foreground.
        assert_eq!(highlight_color(dark, "text.emphasis"), None);
        assert_eq!(diagnostic_color(dark, S::Error), c(0xbf616a)); // NORD11
        assert_eq!(diagnostic_color(dark, S::Hint), c(0xd8dee9)); // NORD4 — fg, not a hue
        assert_eq!(lsp_status_color(dark, &LspStatus::Ready), c(0xa3be8c)); // NORD14
        assert_eq!(lsp_status_color(dark, &LspStatus::Stopped), c(0x4c566a)); // NORD3
                                                                              // Light resolves through its own table.
        assert_ne!(
            highlight_color(ThemeMode::Light, "comment"),
            highlight_color(dark, "comment")
        );
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
        assert_eq!(ui.state_dot(), 13.0);
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
